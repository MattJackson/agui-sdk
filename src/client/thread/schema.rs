//! A deliberately small JSON Schema checker for tool arguments (TOOL-38).
//!
//! It evaluates `type`, `required`, `properties`, `items` (single schema),
//! `enum` and `const`, plus boolean schemas. Annotation keywords that never
//! affect validity (`title`, `description`, `default`, `examples`,
//! `$schema`, `$id`, `$comment`, `deprecated`, `readOnly`, `writeOnly`) are
//! skipped. Every other keyword is reported, with its schema pointer, as not
//! evaluated, so a pass is never mistaken for full validation.
//!
//! The walk is bounded by the schema (the application's own tool
//! definition); the arguments only select which branches apply.

use serde_json::{Map, Value};

use crate::error::{Error, ErrorCode, Result};
use crate::patch::push_token;

const EVALUATED: [&str; 6] = ["type", "required", "properties", "items", "enum", "const"];
const ANNOTATIONS: [&str; 10] = [
    "title",
    "description",
    "default",
    "examples",
    "$schema",
    "$id",
    "$comment",
    "deprecated",
    "readOnly",
    "writeOnly",
];

/// Schema keywords this checker does not evaluate, as pointers into the
/// schema, in document order of the walk.
pub(super) fn unevaluated(schema: &Value) -> Vec<String> {
    let mut out = Vec::new();
    collect(schema, &mut String::new(), &mut out);
    out
}

fn collect(schema: &Value, pointer: &mut String, out: &mut Vec<String>) {
    let Value::Object(map) = schema else {
        if !schema.is_boolean() {
            // Not a schema at all: nothing of it was evaluated.
            out.push(pointer.clone());
        }
        return;
    };
    for (keyword, value) in map {
        let mark = pointer.len();
        push_token(pointer, keyword);
        match keyword.as_str() {
            "properties" => match value {
                Value::Object(properties) => {
                    for (name, sub) in properties {
                        let inner = pointer.len();
                        push_token(pointer, name);
                        collect(sub, pointer, out);
                        pointer.truncate(inner);
                    }
                }
                _ => out.push(pointer.clone()),
            },
            "items" => match value {
                Value::Object(_) | Value::Bool(_) => collect(value, pointer, out),
                // Tuple form (`items: [..]`) is not evaluated.
                _ => out.push(pointer.clone()),
            },
            // Evaluated by name, but only when the value has the shape the
            // checker reads (`check_at`, `check_object`, `type_matches`).
            "type" => match value {
                Value::String(_) => {}
                Value::Array(names) => push_non_strings(names, pointer, out),
                _ => out.push(pointer.clone()),
            },
            "required" => match value {
                Value::Array(names) => push_non_strings(names, pointer, out),
                _ => out.push(pointer.clone()),
            },
            "enum" if !value.is_array() => out.push(pointer.clone()),
            k if EVALUATED.contains(&k) || ANNOTATIONS.contains(&k) => {}
            _ => out.push(pointer.clone()),
        }
        pointer.truncate(mark);
    }
}

/// Reports the non-string entries of a `type` or `required` array.
fn push_non_strings(names: &[Value], pointer: &str, out: &mut Vec<String>) {
    for (index, name) in names.iter().enumerate() {
        if !name.is_string() {
            out.push(format!("{pointer}/{index}"));
        }
    }
}

/// Checks `instance` against the evaluated keywords of `schema`.
pub(super) fn check(schema: &Value, instance: &Value) -> Result<()> {
    let mut pointer = String::new();
    check_at(schema, instance, &mut pointer)
}

fn violation(pointer: &str, what: &str) -> Error {
    let message = if pointer.is_empty() {
        format!("tool arguments at the root {what}")
    } else {
        format!("tool arguments at `{pointer}` {what}")
    };
    Error::new(ErrorCode::ToolArguments, message).with_pointer(pointer)
}

fn check_at(schema: &Value, instance: &Value, pointer: &mut String) -> Result<()> {
    let map = match schema {
        Value::Bool(true) => return Ok(()),
        Value::Bool(false) => return Err(violation(pointer, "are not allowed by the schema")),
        Value::Object(map) => map,
        // Not a schema: listed as unevaluated, nothing to check.
        _ => return Ok(()),
    };
    if let Some(expected) = map.get("type") {
        if !type_matches(expected, instance) {
            return Err(violation(pointer, "have the wrong type"));
        }
    }
    if let Some(Value::Array(allowed)) = map.get("enum") {
        if !allowed.iter().any(|v| json_eq(v, instance)) {
            return Err(violation(pointer, "are not one of the allowed values"));
        }
    }
    if let Some(constant) = map.get("const") {
        if !json_eq(constant, instance) {
            return Err(violation(pointer, "do not equal the required constant"));
        }
    }
    if let Value::Object(object) = instance {
        check_object(map, object, pointer)?;
    }
    if let (Some(items), Value::Array(elements)) = (map.get("items"), instance) {
        if items.is_object() || items.is_boolean() {
            for (index, element) in elements.iter().enumerate() {
                let mark = pointer.len();
                pointer.push('/');
                pointer.push_str(&index.to_string());
                check_at(items, element, pointer)?;
                pointer.truncate(mark);
            }
        }
    }
    Ok(())
}

fn check_object(
    schema: &Map<String, Value>,
    object: &Map<String, Value>,
    pointer: &mut String,
) -> Result<()> {
    if let Some(Value::Array(required)) = schema.get("required") {
        for name in required.iter().filter_map(Value::as_str) {
            if !object.contains_key(name) {
                let mark = pointer.len();
                push_token(pointer, name);
                let err = violation(pointer, "are missing a required property");
                pointer.truncate(mark);
                return Err(err);
            }
        }
    }
    if let Some(Value::Object(properties)) = schema.get("properties") {
        for (name, sub) in properties {
            if let Some(value) = object.get(name) {
                let mark = pointer.len();
                push_token(pointer, name);
                check_at(sub, value, pointer)?;
                pointer.truncate(mark);
            }
        }
    }
    Ok(())
}

fn type_matches(expected: &Value, instance: &Value) -> bool {
    match expected {
        Value::String(name) => is_type(name, instance),
        Value::Array(names) => names
            .iter()
            .filter_map(Value::as_str)
            .any(|name| is_type(name, instance)),
        // A malformed `type` constrains nothing we can read.
        _ => true,
    }
}

fn is_type(name: &str, instance: &Value) -> bool {
    match name {
        "object" => instance.is_object(),
        "array" => instance.is_array(),
        "string" => instance.is_string(),
        "number" => instance.is_number(),
        "integer" => match instance {
            Value::Number(n) => {
                n.is_i64() || n.is_u64() || n.as_f64().is_some_and(|f| f.fract() == 0.0)
            }
            _ => false,
        },
        "boolean" => instance.is_boolean(),
        "null" => instance.is_null(),
        // An unknown type name matches nothing.
        _ => false,
    }
}

/// JSON equality with numbers compared by value (`1` equals `1.0`).
fn json_eq(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Number(x), Value::Number(y)) => match (integer(x), integer(y)) {
            (Some(x), Some(y)) => x == y,
            (Some(i), None) => float_equals_integer(y.as_f64(), i),
            (None, Some(i)) => float_equals_integer(x.as_f64(), i),
            (None, None) => x.as_f64() == y.as_f64(),
        },
        (Value::Array(x), Value::Array(y)) => {
            x.len() == y.len() && x.iter().zip(y).all(|(x, y)| json_eq(x, y))
        }
        (Value::Object(x), Value::Object(y)) => {
            x.len() == y.len()
                && x.iter()
                    .all(|(k, v)| y.get(k).is_some_and(|w| json_eq(v, w)))
        }
        _ => a == b,
    }
}

/// The exact value of an integer number (`i64` or `u64`).
fn integer(n: &serde_json::Number) -> Option<i128> {
    n.as_i64()
        .map(i128::from)
        .or_else(|| n.as_u64().map(i128::from))
}

/// Whether a float is exactly the integer `i`: it must be whole and within
/// the integer range, then compare without rounding the integer to `f64`.
fn float_equals_integer(f: Option<f64>, i: i128) -> bool {
    // -2^63 and 2^64: the range an `i64`/`u64` integer can hold.
    const MIN: f64 = -9_223_372_036_854_775_808.0;
    const END: f64 = 18_446_744_073_709_551_616.0;
    match f {
        // In range and whole, so the cast is exact.
        #[allow(clippy::cast_possible_truncation)]
        Some(f) if f.is_finite() && f.fract() == 0.0 && (MIN..END).contains(&f) => f as i128 == i,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn integer_and_number_equality_by_value() {
        assert!(json_eq(&json!(1), &json!(1.0)));
        assert!(!json_eq(&json!(1), &json!(2)));
        assert!(json_eq(&json!({"a": [1, 2]}), &json!({"a": [1.0, 2]})));
        assert!(!json_eq(&json!({"a": 1}), &json!({"a": 1, "b": 2})));
        assert!(is_type("integer", &json!(3.0)));
        assert!(!is_type("integer", &json!(3.5)));
    }

    /// An integer is compared with a float exactly, not after rounding
    /// both to `f64` (2^53 + 1 is not 2^53).
    #[test]
    fn large_integers_compare_exactly_with_floats() {
        assert!(!json_eq(
            &json!(9_007_199_254_740_993_u64),
            &json!(9_007_199_254_740_992.0)
        ));
        assert!(!json_eq(
            &json!(9_007_199_254_740_992.0),
            &json!(9_007_199_254_740_993_i64)
        ));
        assert!(!json_eq(
            &json!(-9_007_199_254_740_993_i64),
            &json!(-9_007_199_254_740_992.0)
        ));
        assert!(json_eq(
            &json!(9_007_199_254_740_992_u64),
            &json!(9_007_199_254_740_992.0)
        ));
        assert!(json_eq(&json!(-3), &json!(-3.0)));
        assert!(!json_eq(
            &json!(u64::MAX),
            &json!(18_446_744_073_709_551_616.0)
        ));
        assert!(!json_eq(&json!(1), &json!(1.5)));
        assert!(json_eq(&json!(1.5), &json!(1.5)));
    }

    #[test]
    fn pointers_escape_tokens() {
        let mut p = String::new();
        push_token(&mut p, "a/b~c");
        assert_eq!(p, "/a~1b~0c");
    }

    #[test]
    fn unevaluated_lists_unsupported_keywords_only() {
        let schema = json!({
            "type": "object",
            "title": "t",
            "additionalProperties": false,
            "properties": {
                "when": { "type": "string", "format": "date-time" },
                "list": { "items": { "minimum": 1 } },
                "tuple": { "items": [ {} ] }
            }
        });
        let mut got = unevaluated(&schema);
        got.sort();
        assert_eq!(
            got,
            vec![
                "/additionalProperties",
                "/properties/list/items/minimum",
                "/properties/tuple/items",
                "/properties/when/format",
            ]
        );
        assert!(unevaluated(&json!({"type": "string", "description": "d"})).is_empty());
    }

    /// A keyword the checker evaluates by name but whose value it cannot
    /// read constrains nothing, so it is reported rather than passed.
    #[test]
    fn malformed_evaluated_keywords_are_unevaluated() {
        let sorted = |schema: Value| {
            let mut got = unevaluated(&schema);
            got.sort();
            got
        };
        assert_eq!(
            sorted(json!({"type": 5, "required": "a", "enum": {}})),
            ["/enum", "/required", "/type"]
        );
        assert_eq!(
            sorted(json!({"type": ["string", 1], "required": ["a", 2]})),
            ["/required/1", "/type/1"]
        );
        assert!(unevaluated(
            &json!({"type": ["string", "null"], "required": ["a"], "enum": [1], "const": null})
        )
        .is_empty());
    }
}
