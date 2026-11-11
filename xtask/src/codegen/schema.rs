//! Reads `schema.json` (JSON Schema 2020-12) into the model the shape table
//! is emitted from.
//!
//! The reader understands exactly the keyword subset the AG-UI schema uses
//! and refuses anything else, so an upstream schema that starts using a new
//! keyword fails `cargo xtask codegen` instead of being silently
//! mis-compiled. The semantics follow the 2020-12 specification:
//!
//! * `allOf` + `unevaluatedProperties: false` (Core §11.3): the properties a
//!   composed definition accepts are its own plus those its `allOf`
//!   subschemas evaluate, transitively. A name declared on both sides must
//!   satisfy both subschemas, so the two declarations are intersected.
//! * An object schema with neither `unevaluatedProperties` nor
//!   `additionalProperties` accepts undeclared members (Core §10.3.2.3
//!   defaults). Such a definition is a *fragment* when another definition
//!   composes it through `allOf`, and *open* otherwise (the JSON Patch
//!   operations, which RFC 6902 §4 requires to ignore extra members).
//! * `additionalProperties: true` with no `properties` is an object open by
//!   key: every member is data.
//! * `{}` accepts every instance, `null` included; `not: {type: null}`
//!   removes `null` only.
//! * `required` (Validation §6.5.3) lists names that must be present; every
//!   other declared property is optional.
//! * `default` and `contentEncoding` are annotations (Validation §9.2,
//!   §8.3): they never change what validates, and are recorded only as
//!   documentation.

use std::collections::{BTreeMap, BTreeSet};

use super::json::Json;

/// The compiled schema.
#[derive(Debug, Clone, PartialEq)]
pub struct Schema {
    /// The schema's `$id`.
    pub id: String,
    /// The root `$ref` target (the definition a bare document is validated
    /// against).
    pub root: String,
    /// Every `$defs` entry, sorted by name.
    pub defs: Vec<Def>,
}

/// One `$defs` entry.
#[derive(Debug, Clone, PartialEq)]
pub struct Def {
    pub name: String,
    pub kind: DefKind,
}

#[derive(Debug, Clone, PartialEq)]
pub enum DefKind {
    Object(Object),
    Union(Union),
    /// A closed set of strings, sorted.
    Enum(Vec<String>),
    /// Any other single schema.
    Alias(Ty),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Openness {
    Closed,
    Fragment,
    Open,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Object {
    pub openness: Openness,
    /// The definitions composed through `allOf`, transitively, outermost
    /// first.
    pub fragments: Vec<String>,
    /// Declared properties (own and composed), sorted by name.
    pub fields: Vec<Field>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Field {
    pub name: String,
    pub ty: Ty,
    pub required: bool,
    /// `null` validates against the property's schema.
    pub nullable: bool,
    /// The `default` annotation as compact JSON text.
    pub default: Option<String>,
}

impl Field {
    /// The compatibility boundary's null-as-absent rule applies here; see
    /// [`super::shapes`] for the derivation.
    pub fn compat_null(&self) -> bool {
        !self.required && !self.nullable
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Union {
    /// The property every member pins to a distinct constant.
    pub discriminator: String,
    /// `(constant, member definition)`, sorted by constant.
    pub arms: Vec<(String, String)>,
    /// Member definitions in schema order.
    pub members: Vec<String>,
}

/// The type of a property or alias. `null` is described separately
/// ([`Field::nullable`]); nested positions never carry `not`.
#[derive(Debug, Clone, PartialEq)]
pub enum Ty {
    Any,
    String {
        pattern: Option<String>,
    },
    Integer {
        min: Option<i64>,
        max: Option<i64>,
    },
    Boolean,
    Const(String),
    /// A constant that is one member of a closed set defined elsewhere in
    /// the schema (`domain`, a string-enum definition); see `pin`.
    Pinned {
        value: String,
        domain: String,
    },
    Enum(Vec<String>),
    OpenObject,
    Ref(String),
    Array {
        items: Box<Ty>,
        min_items: u64,
    },
    OneOf(Vec<Ty>),
}

const ANNOTATIONS: &[&str] = &["description", "title", "examples", "$comment"];

fn is_annotation(key: &str) -> bool {
    ANNOTATIONS.contains(&key)
}

/// Compiles a parsed schema document.
pub fn compile(doc: &Json) -> Result<Schema, String> {
    let id = doc
        .get("$id")
        .and_then(Json::as_str)
        .ok_or("schema has no string $id")?
        .to_owned();
    match doc.get("$schema").and_then(Json::as_str) {
        Some("https://json-schema.org/draft/2020-12/schema") => {}
        other => return Err(format!("unsupported $schema {other:?}; expected 2020-12")),
    }
    let root = doc
        .get("$ref")
        .and_then(Json::as_str)
        .and_then(|r| r.strip_prefix("#/$defs/"))
        .ok_or("schema has no root `$ref: #/$defs/<name>`")?
        .to_owned();
    let raw_defs = doc
        .get("$defs")
        .and_then(Json::as_object)
        .ok_or("schema has no $defs object")?;
    for (key, _) in doc.as_object().unwrap_or(&[]) {
        if !matches!(key.as_str(), "$schema" | "$id" | "$ref" | "$defs") && !is_annotation(key) {
            return Err(format!("unsupported root keyword {key:?}"));
        }
    }

    let raw: BTreeMap<&str, &Json> = raw_defs.iter().map(|(k, v)| (k.as_str(), v)).collect();
    if !raw.contains_key(root.as_str()) {
        return Err(format!("root $ref names unknown definition {root:?}"));
    }

    // Definitions composed by another through allOf are fragments.
    let mut composed = BTreeSet::new();
    for def in raw.values() {
        if let Some(all_of) = def.get("allOf") {
            for sub in all_of.as_array().ok_or("allOf must be an array")? {
                composed.insert(ref_target(sub)?);
            }
        }
    }

    let reader = Reader {
        raw: &raw,
        composed: &composed,
    };
    let mut defs = Vec::with_capacity(raw.len());
    for (&name, &body) in &raw {
        let kind = reader
            .definition(name, body)
            .map_err(|e| format!("$defs/{name}: {e}"))?;
        defs.push(Def {
            name: name.to_owned(),
            kind,
        });
    }
    let mut schema = Schema { id, root, defs };
    resolve(&mut schema)?;
    Ok(schema)
}

fn ref_target(value: &Json) -> Result<String, String> {
    let obj = value
        .as_object()
        .ok_or("expected a {\"$ref\": ...} subschema")?;
    if obj.iter().any(|(k, _)| k != "$ref" && !is_annotation(k)) {
        return Err("expected a bare $ref subschema".to_owned());
    }
    let target = value
        .get("$ref")
        .and_then(Json::as_str)
        .ok_or("expected a {\"$ref\": ...} subschema")?;
    target
        .strip_prefix("#/$defs/")
        .filter(|name| !name.is_empty() && !name.contains('/'))
        .map(str::to_owned)
        .ok_or_else(|| format!("unsupported $ref {target:?}; only #/$defs/<name> is understood"))
}

struct Reader<'a> {
    raw: &'a BTreeMap<&'a str, &'a Json>,
    composed: &'a BTreeSet<String>,
}

impl Reader<'_> {
    fn definition(&self, name: &str, body: &Json) -> Result<DefKind, String> {
        let members = body.as_object().ok_or("definition is not an object")?;
        if let Some(anchor) = body.get("$anchor") {
            if anchor.as_str() != Some(name) {
                return Err("$anchor differs from the definition name".to_owned());
            }
        }
        let has = |k: &str| body.get(k).is_some();

        if has("oneOf") && members.iter().all(|(k, _)| is_def_meta(k) || k == "oneOf") {
            let arms = body
                .get("oneOf")
                .and_then(Json::as_array)
                .ok_or("oneOf must be an array")?;
            if arms.iter().all(|a| a.get("$ref").is_some()) {
                let members = arms.iter().map(ref_target).collect::<Result<Vec<_>, _>>()?;
                return Ok(DefKind::Union(Union {
                    discriminator: String::new(), // filled by `resolve`
                    arms: Vec::new(),
                    members,
                }));
            }
        }

        if body.get("type").and_then(Json::as_str) == Some("object")
            && (has("properties")
                || has("allOf")
                || has("required")
                || has("unevaluatedProperties"))
        {
            return self.object(name, body).map(DefKind::Object);
        }

        if body.get("type").and_then(Json::as_str) == Some("string") && has("enum") {
            for (k, _) in members {
                if !is_def_meta(k) && !matches!(k.as_str(), "type" | "enum") {
                    return Err(format!("unsupported keyword {k:?} on an enum"));
                }
            }
            return Ok(DefKind::Enum(string_set(body.get("enum"))?));
        }

        let stripped: Vec<(String, Json)> = members
            .iter()
            .filter(|(k, _)| k != "$anchor")
            .cloned()
            .collect();
        let (ty, not_null) = self.subschema(&Json::Object(stripped))?;
        if not_null {
            return Err("`not` is only understood on a property".to_owned());
        }
        Ok(DefKind::Alias(ty))
    }

    fn object(&self, name: &str, body: &Json) -> Result<Object, String> {
        for (k, _) in body.as_object().unwrap_or(&[]) {
            if !is_def_meta(k)
                && !matches!(
                    k.as_str(),
                    "type" | "properties" | "required" | "allOf" | "unevaluatedProperties"
                )
            {
                return Err(format!("unsupported keyword {k:?} on an object"));
            }
        }
        let openness = match body.get("unevaluatedProperties") {
            Some(Json::Bool(false)) => Openness::Closed,
            Some(_) => return Err("only `unevaluatedProperties: false` is understood".to_owned()),
            None if self.composed.contains(name) => Openness::Fragment,
            None => Openness::Open,
        };

        let mut fragments = Vec::new();
        let mut fields: BTreeMap<String, Field> = BTreeMap::new();
        if let Some(all_of) = body.get("allOf") {
            for sub in all_of.as_array().ok_or("allOf must be an array")? {
                let target = ref_target(sub)?;
                let raw = self
                    .raw
                    .get(target.as_str())
                    .ok_or_else(|| format!("allOf names unknown definition {target:?}"))?;
                let inner = self
                    .object(&target, raw)
                    .map_err(|e| format!("allOf {target}: {e}"))?;
                fragments.push(target);
                for f in inner.fragments {
                    if !fragments.contains(&f) {
                        fragments.push(f);
                    }
                }
                for field in inner.fields {
                    self.merge_field(&mut fields, field)?;
                }
            }
        }

        let required = match body.get("required") {
            Some(r) => string_list(Some(r))?,
            None => Vec::new(),
        };
        if let Some(props) = body.get("properties") {
            for (prop, schema) in props.as_object().ok_or("properties must be an object")? {
                let (ty, not_null) = self
                    .subschema(schema)
                    .map_err(|e| format!("property {prop:?}: {e}"))?;
                let default = schema.get("default").map(Json::to_compact);
                self.merge_field(
                    &mut fields,
                    Field {
                        name: prop.clone(),
                        ty,
                        required: false,
                        // Provisional: `resolve` folds in what the type admits.
                        nullable: !not_null,
                        default,
                    },
                )?;
            }
        }
        for req in &required {
            let field = fields
                .get_mut(req)
                .ok_or_else(|| format!("required property {req:?} is not declared"))?;
            field.required = true;
        }
        Ok(Object {
            openness,
            fragments,
            fields: fields.into_values().collect(),
        })
    }

    /// Reads a subschema into its type and whether it carries
    /// `not: {type: null}`.
    fn subschema(&self, schema: &Json) -> Result<(Ty, bool), String> {
        let members = schema.as_object().ok_or("subschema is not an object")?;
        let mut not_null = false;
        let mut rest: Vec<&str> = Vec::new();
        for (k, v) in members {
            match k.as_str() {
                "not" => {
                    if v != &Json::Object(vec![(
                        "type".to_owned(),
                        Json::String("null".to_owned()),
                    )]) {
                        return Err("only `not: {type: null}` is understood".to_owned());
                    }
                    not_null = true;
                }
                "default" | "contentEncoding" => {}
                k if is_annotation(k) => {}
                k => rest.push(k),
            }
        }
        let get = |k: &str| schema.get(k);
        let only = |allowed: &[&str]| -> Result<(), String> {
            match rest.iter().find(|k| !allowed.contains(k)) {
                Some(k) => Err(format!("unsupported keyword {k:?} in this position")),
                None => Ok(()),
            }
        };

        if rest.is_empty() {
            return Ok((Ty::Any, not_null));
        }
        if get("$ref").is_some() {
            only(&["$ref"])?;
            let target = get("$ref")
                .and_then(Json::as_str)
                .and_then(|t| t.strip_prefix("#/$defs/"))
                .ok_or("unsupported $ref")?;
            if !self.raw.contains_key(target) {
                return Err(format!("$ref names unknown definition {target:?}"));
            }
            return Ok((Ty::Ref(target.to_owned()), not_null));
        }
        if let Some(arms) = get("oneOf") {
            only(&["oneOf"])?;
            let mut tys = Vec::new();
            for arm in arms.as_array().ok_or("oneOf must be an array")? {
                let (ty, arm_not_null) = self.subschema(arm)?;
                if arm_not_null {
                    return Err("`not` inside oneOf is not understood".to_owned());
                }
                tys.push(ty);
            }
            return Ok((Ty::OneOf(tys), not_null));
        }
        if let Some(c) = get("const") {
            only(&["const", "type"])?;
            let value = c.as_str().ok_or("only string constants are understood")?;
            if !matches!(get("type").and_then(Json::as_str), None | Some("string")) {
                return Err("a string constant with a non-string type".to_owned());
            }
            return Ok((Ty::Const(value.to_owned()), not_null));
        }
        let ty = match get("type").and_then(Json::as_str) {
            Some("string") if get("enum").is_some() => {
                only(&["type", "enum"])?;
                Ty::Enum(string_set(get("enum"))?)
            }
            Some("string") => {
                only(&["type", "pattern"])?;
                let pattern = match get("pattern") {
                    Some(p) => Some(p.as_str().ok_or("pattern must be a string")?.to_owned()),
                    None => None,
                };
                Ty::String { pattern }
            }
            Some("integer") => {
                only(&["type", "minimum", "maximum"])?;
                Ty::Integer {
                    min: bound(get("minimum"))?,
                    max: bound(get("maximum"))?,
                }
            }
            Some("boolean") => {
                only(&["type"])?;
                Ty::Boolean
            }
            Some("object") => {
                only(&["type", "additionalProperties"])?;
                if get("additionalProperties") != Some(&Json::Bool(true)) {
                    return Err("an inline object must be `additionalProperties: true`".to_owned());
                }
                Ty::OpenObject
            }
            Some("array") => {
                only(&["type", "items", "minItems"])?;
                let (items, items_not_null) =
                    self.subschema(get("items").ok_or("array without items")?)?;
                if items_not_null {
                    return Err("`not` on array items is not understood".to_owned());
                }
                let min_items = match get("minItems") {
                    Some(Json::Number(n)) => n.parse::<u64>().map_err(|_| "bad minItems")?,
                    Some(_) => return Err("bad minItems".to_owned()),
                    None => 0,
                };
                Ty::Array {
                    items: Box::new(items),
                    min_items,
                }
            }
            other => return Err(format!("unsupported type {other:?}")),
        };
        Ok((ty, not_null))
    }
}

fn is_def_meta(key: &str) -> bool {
    key == "$anchor" || is_annotation(key)
}

fn bound(value: Option<&Json>) -> Result<Option<i64>, String> {
    match value {
        None => Ok(None),
        Some(Json::Number(n)) => n
            .parse::<i64>()
            .map(Some)
            .map_err(|_| format!("bound {n} is not an i64 integer")),
        Some(_) => Err("bound is not a number".to_owned()),
    }
}

fn string_list(value: Option<&Json>) -> Result<Vec<String>, String> {
    let items = value
        .and_then(Json::as_array)
        .ok_or("expected an array of strings")?;
    let mut out = Vec::with_capacity(items.len());
    for item in items {
        let s = item.as_str().ok_or("expected an array of strings")?;
        if out.iter().any(|o: &String| o == s) {
            return Err(format!("duplicate entry {s:?}"));
        }
        out.push(s.to_owned());
    }
    Ok(out)
}

fn string_set(value: Option<&Json>) -> Result<Vec<String>, String> {
    let mut out = string_list(value)?;
    out.sort();
    Ok(out)
}

impl Reader<'_> {
    fn merge_field(&self, fields: &mut BTreeMap<String, Field>, new: Field) -> Result<(), String> {
        let Some(old) = fields.get_mut(&new.name) else {
            fields.insert(new.name.clone(), new);
            return Ok(());
        };
        old.ty = self
            .intersect(&old.ty, &new.ty)?
            .ok_or_else(|| format!("cannot intersect the two declarations of {:?}", new.name))?;
        old.nullable = old.nullable && new.nullable;
        old.required = old.required || new.required;
        old.default = match (old.default.take(), new.default) {
            (Some(a), Some(b)) if a != b => {
                return Err(format!("conflicting defaults for {:?}", new.name));
            }
            (a, b) => a.or(b),
        };
        Ok(())
    }

    /// Both declarations apply to one property: the type admitting exactly
    /// the instances both admit, for the combinations the schema uses.
    fn intersect(&self, a: &Ty, b: &Ty) -> Result<Option<Ty>, String> {
        Ok(match (a, b) {
            _ if a == b => Some(a.clone()),
            (Ty::Any, other) | (other, Ty::Any) => Some(other.clone()),
            (Ty::Const(c), Ty::String { pattern: None })
            | (Ty::String { pattern: None }, Ty::Const(c)) => Some(Ty::Const(c.clone())),
            (Ty::Const(c), Ty::Enum(set)) | (Ty::Enum(set), Ty::Const(c)) if set.contains(c) => {
                Some(Ty::Const(c.clone()))
            }
            (Ty::Const(c), Ty::Ref(r)) | (Ty::Ref(r), Ty::Const(c)) => {
                let Some(target) = self.raw.get(r.as_str()) else {
                    return Ok(None);
                };
                match self
                    .definition(r, target)
                    .map_err(|e| format!("$defs/{r}: {e}"))?
                {
                    DefKind::Enum(set) if set.contains(c) => Some(Ty::Const(c.clone())),
                    DefKind::Alias(Ty::String { pattern: None }) => Some(Ty::Const(c.clone())),
                    _ => None,
                }
            }
            _ => None,
        })
    }
}

/// Second pass: settles nullability through references and union
/// discriminators, which need the whole schema.
fn resolve(schema: &mut Schema) -> Result<(), String> {
    let snapshot: BTreeMap<String, DefKind> = schema
        .defs
        .iter()
        .map(|d| (d.name.clone(), d.kind.clone()))
        .collect();

    for def in &mut schema.defs {
        if let DefKind::Object(obj) = &mut def.kind {
            for field in &mut obj.fields {
                field.nullable = field.nullable && admits_null(&field.ty, &snapshot);
            }
        }
    }

    for def in &mut schema.defs {
        let name = def.name.clone();
        if let DefKind::Union(union) = &mut def.kind {
            let (discriminator, arms) = discriminate(&union.members, &snapshot)
                .map_err(|e| format!("$defs/{name}: {e}"))?;
            union.discriminator = discriminator;
            union.arms = arms;
        }
    }
    pin(schema);
    Ok(())
}

/// Third pass: marks the constants that are one value of a closed set.
///
/// A property `p` declared as `const: v` is *pinned* when the schema has a
/// string-enum definition named after the property (`role` -> `Role`) that
/// lists `v`, and the property is not the discriminator of a union the
/// object belongs to. `REASONING_MESSAGE_START.role` (`const: "reasoning"`)
/// is the case in point: `"reasoning"` is one of the `Role` values.
///
/// The distinction matters on receipt (D3, PROC-17). A value of a closed set
/// that this build does not know is unrecognised material and is stripped;
/// a value it does know, but that is not the pinned one (`role: "user"` on
/// a reasoning stream), is a contradiction and fatal. A plain constant has
/// no such set, so any other value is malformed. A union discriminator is
/// excluded because the union has already chosen the member by that value,
/// so it can only ever match.
fn pin(schema: &mut Schema) {
    let enums: BTreeMap<String, Vec<String>> = schema
        .defs
        .iter()
        .filter_map(|d| match &d.kind {
            DefKind::Enum(set) => Some((d.name.clone(), set.clone())),
            _ => None,
        })
        .collect();
    let discriminators: BTreeSet<(String, String)> = schema
        .defs
        .iter()
        .filter_map(|d| match &d.kind {
            DefKind::Union(u) => Some(u),
            _ => None,
        })
        .flat_map(|u| {
            u.members
                .iter()
                .map(|m| (m.clone(), u.discriminator.clone()))
        })
        .collect();
    for def in &mut schema.defs {
        let DefKind::Object(obj) = &mut def.kind else {
            continue;
        };
        for field in &mut obj.fields {
            let Ty::Const(value) = &field.ty else {
                continue;
            };
            if discriminators.contains(&(def.name.clone(), field.name.clone())) {
                continue;
            }
            let domain = upper_first(&field.name);
            if enums.get(&domain).is_some_and(|set| set.contains(value)) {
                field.ty = Ty::Pinned {
                    value: value.clone(),
                    domain,
                };
            }
        }
    }
}

fn upper_first(name: &str) -> String {
    let mut chars = name.chars();
    match chars.next() {
        Some(c) => c.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}

fn admits_null(ty: &Ty, defs: &BTreeMap<String, DefKind>) -> bool {
    match ty {
        Ty::Any => true,
        Ty::OneOf(arms) => arms.iter().any(|t| admits_null(t, defs)),
        Ty::Ref(name) => match defs.get(name) {
            Some(DefKind::Alias(inner)) => admits_null(inner, defs),
            _ => false,
        },
        _ => false,
    }
}

/// Finds the one property every member pins to a distinct constant.
fn discriminate(
    members: &[String],
    defs: &BTreeMap<String, DefKind>,
) -> Result<(String, Vec<(String, String)>), String> {
    let mut candidates: Option<BTreeSet<String>> = None;
    let mut consts: Vec<BTreeMap<String, String>> = Vec::new();
    for member in members {
        let Some(DefKind::Object(obj)) = defs.get(member) else {
            return Err(format!("union member {member} is not an object definition"));
        };
        let pinned: BTreeMap<String, String> = obj
            .fields
            .iter()
            .filter(|f| f.required)
            .filter_map(|f| match &f.ty {
                Ty::Const(c) => Some((f.name.clone(), c.clone())),
                _ => None,
            })
            .collect();
        let names: BTreeSet<String> = pinned.keys().cloned().collect();
        candidates = Some(match candidates {
            None => names,
            Some(c) => c.intersection(&names).cloned().collect(),
        });
        consts.push(pinned);
    }
    let candidates: Vec<String> = candidates
        .unwrap_or_default()
        .into_iter()
        .filter(|prop| {
            let values: BTreeSet<&String> = consts.iter().filter_map(|c| c.get(prop)).collect();
            values.len() == members.len()
        })
        .collect();
    let [prop] = candidates.as_slice() else {
        return Err(format!(
            "expected exactly one discriminating property, found {candidates:?}"
        ));
    };
    let mut arms: Vec<(String, String)> = members
        .iter()
        .zip(&consts)
        .filter_map(|(m, c)| c.get(prop).map(|v| (v.clone(), m.clone())))
        .collect();
    arms.sort();
    Ok((prop.clone(), arms))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn intersect_reports_underlying_ref_error() -> Result<(), String> {
        let doc = super::super::json::parse(
            r##"{"$schema":"https://json-schema.org/draft/2020-12/schema","$id":"t",
            "$ref":"#/$defs/A","$defs":{
            "A":{"type":"object","allOf":[{"$ref":"#/$defs/Base"}],
                 "properties":{"kind":{"$ref":"#/$defs/Kind"}}},
            "Base":{"type":"object","properties":{"kind":{"const":"x"}}},
            "Kind":{"type":"string","enum":["x"],"format":"bogus"}}}"##,
        )
        .map_err(|e| format!("{e:?}"))?;
        match compile(&doc) {
            Err(err) => {
                assert!(err.contains("unsupported keyword \"format\""), "{err}");
                Ok(())
            }
            Ok(_) => Err("Kind is unsupported, compile must fail".to_owned()),
        }
    }
}
