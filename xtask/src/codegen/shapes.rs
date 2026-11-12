//! Emits `src/pipeline/shapes.rs` from the compiled schema.

use std::fmt::Write as _;

use super::schema::{Def, DefKind, Field, Openness, Schema, Ty};
use super::{header, screaming_snake, string_literal};

/// The module documentation, including the derivation of the
/// compatibility boundary's null-as-absent positions.
const MODULE_DOC: &str = r#"
//! The enforcement shape table: every `$defs` entry of the pinned AG-UI
//! schema compiled into `static` data, so enforcement walks it without
//! allocating or initialising anything at run time.
//!
//! * [`DEFS`] holds every definition sorted by name; a [`DefId`] indexes it
//!   and there is one named `DefId` constant per definition. [`lookup`]
//!   finds a definition by name and [`event`] an event definition by its
//!   `type` value, both by binary search.
//! * An [`Object`]'s fields include those it composes through `allOf`
//!   (JSON Schema 2020-12 `unevaluatedProperties` sees them), sorted by
//!   wire name for [`Object::field`]. A property declared both in a fragment
//!   and in the composing definition holds the intersection of the two
//!   (`type: EventType` and `const: "RUN_STARTED"` become the constant).
//! * [`Openness`] says what an undeclared member is: unknown material in a
//!   closed object, nothing at all in a fragment (only ever validated as
//!   part of a closed composer) or in an open object (the JSON Patch
//!   operations: RFC 6902 §4 makes extra members ignorable).
//! * A [`Union`] is a `oneOf` over object definitions, keyed by the one
//!   property every member pins to a distinct constant.
//! * A constant property named after a string-enum definition that lists
//!   its value (`role: "reasoning"` and `Role`) is [`Ty::Pinned`], unless
//!   it is the discriminator of a union the object belongs to: a different
//!   known value contradicts it (fatal), an unknown one is unrecognised
//!   material (stripped), as D3 / PROC-17 require.
//! * [`Ty::Any`], [`Ty::OpenObject`] and objects open by key carry data,
//!   never unknown fields.
//!
//! # `null` in an optional position
//!
//! [`Field::nullable`] says whether `null` validates against the property's
//! schema: true for required JSON payloads such as `CUSTOM.value` and
//! `STATE_SNAPSHOT.snapshot`, false wherever the type excludes it or the
//! schema says `not: {type: null}` (the optional any-JSON properties).
//!
//! [`Field::compat_null`] marks where the compatibility boundary reads a
//! whole `null` as the field being absent (BASIC-12). It is derived, not
//! listed: it is set exactly on the fields that are optional and not
//! nullable. The reasoning, from the 1.0 prose:
//!
//! * `basic/index.mdx`, "Absent means absent": an optional field with no
//!   value MUST be omitted rather than sent as `null`, "a whole optional
//!   field set to `null` MUST be omitted", and this covers the any-JSON
//!   fields too (`rawEvent`, `RUN_FINISHED.result`, `forwardedProps`). So
//!   in every optional position a whole `null` can only ever mean "absent";
//!   no optional field has a second meaning for it to collide with.
//! * The same section: "Where the protocol has already shipped tolerance
//!   for a `null` in an optional position, it is handled by a compatibility
//!   shim rather than by this rule." Producers that predate the rule exist,
//!   and nothing in the 1.0 text narrows which optional positions they may
//!   have used, so the shim covers all of them rather than a remembered
//!   subset.
//! * A required position is never in the set. A required payload that
//!   admits `null` keeps it as data (`CUSTOM.value`); one that does not is
//!   malformed, and reading it as absent would still fail (a missing
//!   required field), so the shim could only blur the error.
//! * A `null` under an open-by-key object (a metadata value, anything
//!   inside `state`) is data and MUST be preserved. Those are members of
//!   [`Ty::OpenObject`] or [`Ty::Any`] values, not declared properties, so
//!   they never carry the flag.
//! * The flag is for the receive-path boundary only. The strict validator
//!   ignores it: the value corpus pins `rawEvent: null`,
//!   `parentMessageId: null`, `subagentRunId: null`, `code: null`,
//!   `metadata: null` and `outcome: null` as invalid 1.0 documents, which
//!   they remain.
"#;

/// Renders the module (before rustfmt).
pub fn render(schema: &Schema, commit: &str) -> Result<String, String> {
    let mut out = String::new();
    out.push_str(&header(super::SCHEMA_FILE, commit));
    out.push_str(MODULE_DOC.trim_start());
    out.push('\n');
    let _ = writeln!(
        out,
        "/// The `$id` of the schema this table was generated from.\n\
         pub const SCHEMA_ID: &str = {};",
        string_literal(&schema.id)
    );
    let _ = writeln!(
        out,
        "/// The upstream commit the schema was read at (`spec.lock`).\n\
         pub const SPEC_COMMIT: &str = {};",
        string_literal(commit)
    );
    let _ = writeln!(
        out,
        "/// The definition a bare document is validated against (the schema's root `$ref`).\n\
         pub const ROOT: DefId = {};",
        screaming_snake(&schema.root)
    );
    match schema
        .defs
        .iter()
        .find(|d| d.name == schema.root)
        .map(|d| &d.kind)
    {
        Some(DefKind::Union(u)) if u.discriminator == "type" => {}
        _ => return Err("the root definition is not a union discriminated by `type`".to_owned()),
    }
    out.push_str(TYPES);

    let index = |name: &str| -> Result<String, String> {
        if schema.defs.iter().any(|d| d.name == name) {
            Ok(screaming_snake(name))
        } else {
            Err(format!("unknown definition {name:?}"))
        }
    };

    let _ = writeln!(
        out,
        "/// Every definition, sorted by name. A [`DefId`] is an index into this array.\n\
         pub static DEFS: [Def; {}] = [",
        schema.defs.len()
    );
    for def in &schema.defs {
        emit_def(&mut out, def, &index)?;
    }
    out.push_str("];\n\n");

    for (i, def) in schema.defs.iter().enumerate() {
        let _ = writeln!(
            out,
            "/// `#/$defs/{}`.\npub const {}: DefId = DefId({i});",
            def.name,
            screaming_snake(&def.name)
        );
    }
    if schema.defs.len() > usize::from(u16::MAX) {
        return Err("too many definitions for a u16 index".to_owned());
    }

    out.push_str(
        "\n/// Every field the compatibility boundary reads a whole `null` in as absent\n\
         /// (see the module documentation), as `(definition, field)`, sorted.\n\
         pub static COMPAT_NULL_FIELDS: &[(&str, &str)] = &[\n",
    );
    for def in &schema.defs {
        if let DefKind::Object(obj) = &def.kind {
            if obj.openness == Openness::Fragment {
                continue;
            }
            for field in obj.fields.iter().filter(|f| f.compat_null()) {
                let _ = writeln!(
                    out,
                    "({}, {}),",
                    string_literal(&def.name),
                    string_literal(&field.name)
                );
            }
        }
    }
    out.push_str("];\n");
    out.push_str(LOOKUPS);
    Ok(out)
}

fn emit_def(
    out: &mut String,
    def: &Def,
    index: &dyn Fn(&str) -> Result<String, String>,
) -> Result<(), String> {
    let _ = write!(out, "Def {{ name: {}, kind: ", string_literal(&def.name));
    match &def.kind {
        DefKind::Object(obj) => {
            let openness = match obj.openness {
                Openness::Closed => "Closed",
                Openness::Fragment => "Fragment",
                Openness::Open => "Open",
            };
            let fragments = obj
                .fragments
                .iter()
                .map(|f| index(f))
                .collect::<Result<Vec<_>, _>>()?
                .join(", ");
            let _ = write!(
                out,
                "DefKind::Object(Object {{ openness: Openness::{openness}, fragments: &[{fragments}], fields: &["
            );
            for field in &obj.fields {
                emit_field(out, field, index)?;
            }
            out.push_str("] })");
        }
        DefKind::Union(union) => {
            let members = union
                .members
                .iter()
                .map(|m| index(m))
                .collect::<Result<Vec<_>, _>>()?
                .join(", ");
            let _ = write!(
                out,
                "DefKind::Union(Union {{ discriminator: {}, members: &[{members}], arms: &[",
                string_literal(&union.discriminator)
            );
            for (value, member) in &union.arms {
                let _ = write!(
                    out,
                    "Arm {{ value: {}, def: {} }},",
                    string_literal(value),
                    index(member)?
                );
            }
            out.push_str("] })");
        }
        DefKind::Enum(values) => {
            let _ = write!(out, "DefKind::Enum({})", str_slice(values));
        }
        DefKind::Alias(ty) => {
            let _ = write!(out, "DefKind::Alias({})", ty_expr(ty, index)?);
        }
    }
    out.push_str(" },\n");
    Ok(())
}

fn emit_field(
    out: &mut String,
    field: &Field,
    index: &dyn Fn(&str) -> Result<String, String>,
) -> Result<(), String> {
    let default = match &field.default {
        Some(d) => format!("Some({})", string_literal(d)),
        None => "None".to_owned(),
    };
    let _ = write!(
        out,
        "Field {{ name: {}, ty: {}, required: {}, nullable: {}, compat_null: {}, default: {default} }},",
        string_literal(&field.name),
        ty_expr(&field.ty, index)?,
        field.required,
        field.nullable,
        field.compat_null(),
    );
    Ok(())
}

fn str_slice(values: &[String]) -> String {
    let items: Vec<String> = values.iter().map(|v| string_literal(v)).collect();
    format!("&[{}]", items.join(", "))
}

fn opt_i64(v: Option<i64>) -> String {
    match v {
        Some(n) => format!("Some({n})"),
        None => "None".to_owned(),
    }
}

fn ty_expr(ty: &Ty, index: &dyn Fn(&str) -> Result<String, String>) -> Result<String, String> {
    Ok(match ty {
        Ty::Any => "Ty::Any".to_owned(),
        Ty::String { pattern: None } => "Ty::String { pattern: None }".to_owned(),
        Ty::String { pattern: Some(p) } => {
            format!("Ty::String {{ pattern: Some({}) }}", string_literal(p))
        }
        Ty::Integer { min, max } => {
            format!(
                "Ty::Integer {{ min: {}, max: {} }}",
                opt_i64(*min),
                opt_i64(*max)
            )
        }
        Ty::Boolean => "Ty::Boolean".to_owned(),
        Ty::Const(c) => format!("Ty::Const({})", string_literal(c)),
        Ty::Pinned { value, domain } => format!(
            "Ty::Pinned {{ value: {}, domain: {} }}",
            string_literal(value),
            index(domain)?
        ),
        Ty::Enum(values) => format!("Ty::Enum({})", str_slice(values)),
        Ty::OpenObject => "Ty::OpenObject".to_owned(),
        Ty::Ref(name) => format!("Ty::Ref({})", index(name)?),
        Ty::Array { items, min_items } => format!(
            "Ty::Array {{ items: &{}, min_items: {min_items} }}",
            ty_expr(items, index)?
        ),
        Ty::OneOf(arms) => {
            let arms = arms
                .iter()
                .map(|a| ty_expr(a, index))
                .collect::<Result<Vec<_>, _>>()?;
            format!("Ty::OneOf(&[{}])", arms.join(", "))
        }
    })
}

const TYPES: &str = r#"
/// Identifies a definition: an index into [`DEFS`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct DefId(pub u16);

impl DefId {
    /// The definition this id names.
    pub fn def(self) -> &'static Def {
        &DEFS[usize::from(self.0)]
    }

    /// The definition's `$defs` name.
    pub fn name(self) -> &'static str {
        self.def().name
    }
}

/// One `$defs` entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Def {
    /// The `$defs` key (also its `$anchor`).
    pub name: &'static str,
    /// What the definition is.
    pub kind: DefKind,
}

/// The shapes a definition can take.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DefKind {
    /// An object with declared properties.
    Object(Object),
    /// A `oneOf` over object definitions with a discriminating property.
    Union(Union),
    /// A closed set of strings, sorted (binary-searchable with [`contains`]).
    Enum(&'static [&'static str]),
    /// Any other single schema.
    Alias(Ty),
}

/// What an undeclared member of an object is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Openness {
    /// `unevaluatedProperties: false`: an undeclared member is unknown
    /// material.
    Closed,
    /// A compositional fragment, open in itself and only ever validated as
    /// part of the closed definitions composing it.
    Fragment,
    /// Open: undeclared members are ignored (the JSON Patch operations,
    /// RFC 6902 §4).
    Open,
}

/// An object definition.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Object {
    /// Closed, or how it is open.
    pub openness: Openness,
    /// The fragments composed through `allOf`, transitively, outermost first.
    pub fragments: &'static [DefId],
    /// Every declared property, composed ones included, sorted by wire name.
    pub fields: &'static [Field],
}

impl Object {
    /// The declared property with this wire name (binary search).
    pub fn field(&self, name: &str) -> Option<&'static Field> {
        self.fields
            .binary_search_by(|f| f.name.cmp(name))
            .ok()
            .and_then(|i| self.fields.get(i))
    }
}

/// One declared property.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Field {
    /// The wire name.
    pub name: &'static str,
    /// The type of a non-`null` value.
    pub ty: Ty,
    /// Listed in `required`.
    pub required: bool,
    /// `null` validates against the property's schema (a JSON payload that
    /// may be `null`, such as `CUSTOM.value`).
    pub nullable: bool,
    /// The compatibility boundary reads a whole `null` here as absent (see
    /// the module documentation). Set exactly when the field is optional and
    /// not nullable.
    pub compat_null: bool,
    /// The `default` annotation, as compact JSON text. Documentation only:
    /// an absent field stays absent.
    pub default: Option<&'static str>,
}

/// A union of object definitions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Union {
    /// The property every member pins to a distinct constant.
    pub discriminator: &'static str,
    /// The members, in schema order.
    pub members: &'static [DefId],
    /// Discriminator value to member, sorted by value.
    pub arms: &'static [Arm],
}

impl Union {
    /// The member a discriminator value selects; `None` for a value no
    /// member pins (an unknown union member).
    pub fn arm(&self, value: &str) -> Option<DefId> {
        self.arms
            .binary_search_by(|a| a.value.cmp(value))
            .ok()
            .and_then(|i| self.arms.get(i))
            .map(|a| a.def)
    }
}

/// One union arm.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Arm {
    /// The discriminator value.
    pub value: &'static str,
    /// The member it selects.
    pub def: DefId,
}

/// The type of a property, array item, `oneOf` arm or alias.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ty {
    /// Any JSON value. Whether `null` is among them is the field's
    /// [`Field::nullable`]; in a nested position it is.
    Any,
    /// A string.
    String {
        /// An ECMA-262 regular expression the string must match.
        pattern: Option<&'static str>,
    },
    /// A JSON number with no fractional part (JSON Schema `integer`).
    Integer {
        /// Inclusive minimum.
        min: Option<i64>,
        /// Inclusive maximum.
        max: Option<i64>,
    },
    /// `true` or `false`.
    Boolean,
    /// Exactly this string.
    Const(&'static str),
    /// Exactly `value`, which is one member of the closed set `domain` (a
    /// [`DefKind::Enum`]), as `REASONING_MESSAGE_START.role` is one of the
    /// `Role` values. Another member of the set contradicts the constant;
    /// a value outside the set is unrecognised material (D3, PROC-17).
    Pinned {
        /// The constant.
        value: &'static str,
        /// The closed set it belongs to.
        domain: DefId,
    },
    /// One of a closed, sorted set of strings.
    Enum(&'static [&'static str]),
    /// An object open by key: every member is data.
    OpenObject,
    /// The named definition.
    Ref(DefId),
    /// An array.
    Array {
        /// The item type.
        items: &'static Ty,
        /// Minimum length.
        min_items: u64,
    },
    /// Exactly one of these.
    OneOf(&'static [Ty]),
}

/// Whether a sorted string set (an [`DefKind::Enum`] or [`Ty::Enum`])
/// contains `value` (binary search).
pub fn contains(set: &[&str], value: &str) -> bool {
    set.binary_search(&value).is_ok()
}

"#;

const LOOKUPS: &str = r#"
/// The definition with this `$defs` name (binary search over [`DEFS`]).
pub fn lookup(name: &str) -> Option<DefId> {
    DEFS.binary_search_by(|d| d.name.cmp(name))
        .ok()
        .and_then(|i| u16::try_from(i).ok())
        .map(DefId)
}

/// The event definition for a `type` value; `None` for an unknown event
/// type.
pub fn event(event_type: &str) -> Option<DefId> {
    match ROOT.def().kind {
        DefKind::Union(union) => union.arm(event_type),
        _ => None,
    }
}
"#;
