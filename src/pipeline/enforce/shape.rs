//! The shape model enforcement walks: what each protocol position accepts.
//!
//! A [`Shape`] is a view of one position in the generated schema table
//! (`pipeline::shapes`, written by `cargo xtask codegen`). The views are
//! small `Copy` values that borrow the `static` table, built on demand as the
//! walk descends: nothing is initialised at run time and nothing allocates.
//! Objects list their members sorted by name and unions their arms sorted by
//! discriminator value. A lookup is a linear scan of that short slice that
//! compares length and first byte before any text (see `position`).
//!
//! The table says what JSON Schema says; this module adds the reading the
//! receive path needs:
//!
//! * a property's `nullable` decides whether an any-JSON position takes
//!   `null` ([`Shape::Any`]) or not ([`Shape::NotNull`]); nested positions
//!   (array items, aliases reached from them) take it;
//! * a pattern-constrained string is an RFC 6901 pointer ([`Shape::Pointer`]),
//!   the only pattern the schema uses (a unit test holds that);
//! * `oneOf` a string and an array is [`Shape::TextOrList`], the only `oneOf`
//!   in a property position (also held by a test);
//! * which diagnostic an unknown union member gets ([`Unknown`]) follows from
//!   which union it is.

use crate::diagnostics::DiagnosticCode;
use crate::pipeline::shapes::{self, DefId, DefKind, Openness, Ty};

/// What one position in a document accepts.
#[derive(Debug, Clone, Copy)]
pub(crate) enum Shape {
    /// Any JSON value, `null` included (state, `CUSTOM.value`, a patch `value`).
    Any,
    /// Any JSON value except `null` (`rawEvent`, `result`, `forwardedProps`).
    NotNull,
    /// A string.
    Str,
    /// An RFC 6901 JSON pointer.
    Pointer,
    /// `true` or `false`.
    Bool,
    /// An integer within `min..=max`.
    Int { min: i64, max: i64 },
    /// Exactly this string. A different value is malformed, not unrecognised.
    Const(&'static str),
    /// One of a closed set of strings. An unrecognised member is stripped.
    Enum(&'static [&'static str]),
    /// A value of the closed set `set` pinned to `value` (a role literal such
    /// as `REASONING_MESSAGE_START.role`). Another member of the set is
    /// malformed; a value outside the set is unrecognised material (D3).
    Pinned {
        value: &'static str,
        set: &'static [&'static str],
    },
    /// A closed set `set` drawn from the wider closed set `domain` (the
    /// streamed text roles out of every message role). Another member of
    /// `domain` is a known value in the wrong place, so malformed; a value
    /// outside `domain` is unrecognised material (D3).
    Restricted {
        set: &'static [&'static str],
        domain: &'static [&'static str],
    },
    /// An object open by key: every member is preserved.
    OpenObject,
    /// A protocol object.
    Object(Obj),
    /// An array.
    List(List),
    /// A string, or an array of content parts.
    TextOrList(List),
    /// A discriminated union of protocol objects.
    Union(Union),
}

impl Shape {
    /// Whether `null` is a legal value here.
    pub(crate) fn accepts_null(self) -> bool {
        matches!(self, Shape::Any)
    }

    /// The shape of a definition, `nullable` saying whether an any-JSON
    /// alias takes `null` in this position.
    pub(crate) fn of_def(id: DefId, nullable: bool) -> Shape {
        match id.def().kind {
            DefKind::Object(ref o) => Shape::Object(Obj::new(id, o)),
            DefKind::Union(ref u) => Shape::Union(Union::new(id, u)),
            DefKind::Enum(set) => match restricted_domain(id) {
                Some(domain) => Shape::Restricted { set, domain },
                None => Shape::Enum(set),
            },
            DefKind::Alias(ref ty) => Shape::of_ty(ty, nullable),
        }
    }

    /// The shape of a type in the table.
    fn of_ty(ty: &'static Ty, nullable: bool) -> Shape {
        match *ty {
            Ty::Any if nullable => Shape::Any,
            Ty::Any => Shape::NotNull,
            Ty::String { pattern: None } => Shape::Str,
            Ty::String { pattern: Some(_) } => Shape::Pointer,
            Ty::Integer { min, max } => Shape::Int {
                min: min.unwrap_or(i64::MIN),
                max: max.unwrap_or(i64::MAX),
            },
            Ty::Boolean => Shape::Bool,
            Ty::Const(c) => Shape::Const(c),
            Ty::Pinned { value, domain } => match domain.def().kind {
                DefKind::Enum(set) => Shape::Pinned { value, set },
                // The generator only pins to an enum; anything else is
                // read as the plain constant.
                _ => Shape::Const(value),
            },
            Ty::Enum(set) => Shape::Enum(set),
            Ty::OpenObject => Shape::OpenObject,
            Ty::Ref(id) => Shape::of_def(id, nullable),
            Ty::Array { items, min_items } => Shape::List(List::new(items, min_items)),
            Ty::OneOf(arms) => match text_or_list(arms) {
                Some(list) => Shape::TextOrList(list),
                // No other `oneOf` exists (unit-tested); accept what any
                // JSON would rather than refuse valid documents.
                None if nullable => Shape::Any,
                None => Shape::NotNull,
            },
        }
    }
}

/// The wider set an enum definition restricts, when the schema carves it
/// out of one: `TextMessageRole` is `Role` without the roles a streamed text
/// message cannot take (`tool`, `reasoning`, `activity`). A role is protocol
/// vocabulary, not material from a newer version, so one of those there is
/// malformed, as a wrong role on `REASONING_MESSAGE_START` is (decision D8).
fn restricted_domain(id: DefId) -> Option<&'static [&'static str]> {
    if id != shapes::TEXT_MESSAGE_ROLE {
        return None;
    }
    match shapes::ROLE.def().kind {
        DefKind::Enum(domain) => Some(domain),
        _ => None,
    }
}

/// `oneOf: [string, array]` in either order.
fn text_or_list(arms: &'static [Ty]) -> Option<List> {
    match arms {
        [Ty::String { pattern: None }, Ty::Array { items, min_items }]
        | [Ty::Array { items, min_items }, Ty::String { pattern: None }] => {
            Some(List::new(items, *min_items))
        }
        _ => None,
    }
}

/// One member of a protocol object.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Field {
    pub(crate) name: &'static str,
    pub(crate) required: bool,
    pub(crate) shape: Shape,
}

impl Field {
    fn new(f: &'static shapes::Field) -> Field {
        Field {
            name: f.name,
            required: f.required,
            shape: Shape::of_ty(&f.ty, f.nullable),
        }
    }
}

/// A protocol object. `closed` objects strip undescribed members; open ones
/// (the JSON Patch operations, RFC 6902 §4) keep them.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Obj {
    pub(crate) anchor: &'static str,
    pub(crate) closed: bool,
    /// The fragments composed into it, for the optional-null list.
    fragments: &'static [DefId],
    /// Sorted by name; at most 64, so a `u64` tracks which were seen
    /// (unit-tested).
    members: &'static [shapes::Field],
    /// A union variant whose discriminator is not the object's first member:
    /// the byte walk presents the discriminator (`tag`, `value`) first, so a
    /// tagged visitor need not buffer the members that precede it.
    pub(crate) hoist: Option<(&'static str, &'static str)>,
}

impl Obj {
    fn new(id: DefId, o: &'static shapes::Object) -> Obj {
        Obj {
            anchor: id.name(),
            closed: o.openness == Openness::Closed,
            fragments: o.fragments,
            members: o.fields,
            hoist: None,
        }
    }

    /// Whether a whole `null` for the member `field` is on the compatibility
    /// boundary's optional-null list, and so read as absent rather than
    /// rejected (BASIC-12). Asked only when a `null` is met.
    pub(crate) fn null_reads_absent(&self, field: &str) -> bool {
        crate::pipeline::compat::null_is_listed(self.anchor, self.fragments, field)
    }

    /// The member named `key`, with its index in the member list.
    pub(crate) fn find(&self, key: &str) -> Option<(usize, Field)> {
        self.find_bytes(key.as_bytes())
    }

    /// [`find`](Self::find) by the name's bytes.
    pub(crate) fn find_bytes(&self, key: &[u8]) -> Option<(usize, Field)> {
        position(self.members, |f| f.name, key).map(|i| (i, Field::new(&self.members[i])))
    }

    /// The members, sorted by name.
    pub(crate) fn fields(&self) -> impl Iterator<Item = Field> {
        self.members.iter().map(Field::new)
    }
}

/// Where `key` is among the names of `items` (distinct, none empty).
///
/// A scan that compares lengths and first bytes before any text: the names
/// are few (at most 64 members, about 30 event types) and short, and they
/// share long prefixes (`TEXT_MESSAGE_`, `TOOL_CALL_`), so a binary search
/// pays an out-of-line comparison of the common prefix at every step, where
/// this compares text about once, for the match.
#[inline]
fn position<T>(
    items: &'static [T],
    name: impl Fn(&T) -> &'static str,
    key: &[u8],
) -> Option<usize> {
    let first = key.first()?;
    items.iter().position(|item| {
        let n = name(item).as_bytes();
        n.len() == key.len() && n.first() == Some(first) && n == key
    })
}

/// An array and its minimum length.
#[derive(Debug, Clone, Copy)]
pub(crate) struct List {
    items: &'static Ty,
    pub(crate) min_items: usize,
}

impl List {
    fn new(items: &'static Ty, min_items: u64) -> List {
        List {
            items,
            min_items: usize::try_from(min_items).unwrap_or(usize::MAX),
        }
    }

    /// What each element accepts. Array items are a nested position, so an
    /// any-JSON item takes `null`.
    pub(crate) fn items(&self) -> Shape {
        Shape::of_ty(self.items, true)
    }
}

/// What an unrecognised member of a union is, for its diagnostic.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Unknown {
    /// A whole event of an unknown type.
    Event,
    /// A content part, source, message or outcome of an unknown kind.
    Variant,
    /// A JSON Patch operation with an unknown `op`.
    PatchOp,
}

impl Unknown {
    pub(crate) fn code(self) -> DiagnosticCode {
        match self {
            Unknown::Event => DiagnosticCode::UnknownEvent,
            Unknown::Variant => DiagnosticCode::UnknownVariant,
            Unknown::PatchOp => DiagnosticCode::UnknownPatchOp,
        }
    }

    fn of(id: DefId) -> Unknown {
        if id == shapes::ROOT {
            Unknown::Event
        } else if id == shapes::JSON_PATCH_OPERATION {
            Unknown::PatchOp
        } else {
            Unknown::Variant
        }
    }
}

/// A union of objects discriminated by the string member `tag`.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Union {
    pub(crate) anchor: &'static str,
    pub(crate) tag: &'static str,
    pub(crate) unknown: Unknown,
    arms: &'static [shapes::Arm],
}

impl Union {
    fn new(id: DefId, u: &'static shapes::Union) -> Union {
        Union {
            anchor: id.name(),
            tag: u.discriminator,
            unknown: Unknown::of(id),
            arms: u.arms,
        }
    }

    /// The variant whose discriminator is `value`.
    pub(crate) fn variant(&self, value: &str) -> Option<(&'static str, Obj)> {
        let arm = &self.arms[position(self.arms, |a| a.value, value.as_bytes())?];
        match arm.def.def().kind {
            DefKind::Object(ref o) => Some((arm.value, Obj::new(arm.def, o))),
            _ => None,
        }
    }

    /// How many variants the union has.
    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.arms.len()
    }
}

/// The event union (the schema's root).
pub(crate) fn event_union() -> Union {
    match Shape::of_def(shapes::ROOT, false) {
        Shape::Union(u) => u,
        // The generator refuses a root that is not a union.
        _ => unreachable!("the schema root is a union"),
    }
}

/// The shape registered under a `$defs` anchor. A bare document may be any
/// JSON, so an any-JSON alias takes `null`. Fragments (`BaseEvent`,
/// `BaseMessage`, `Attributable`) are not anchors: they are only ever
/// validated as part of the closed definitions composing them.
pub(crate) fn anchor(name: &str) -> Option<Shape> {
    let id = shapes::lookup(name)?;
    match id.def().kind {
        DefKind::Object(o) if o.openness == Openness::Fragment => None,
        _ => Some(Shape::of_def(id, true)),
    }
}
