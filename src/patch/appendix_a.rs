//! RFC 6902 Appendix A examples, run through [`super::apply`].

#![allow(clippy::unwrap_used, clippy::expect_used)]

use serde_json::{json, Value};

use crate::patch::{apply, PatchBudget, PatchErrorReason, PatchLimits, PatchOperation};

fn run(doc: Value, patch: Value) -> Result<Value, PatchErrorReason> {
    let ops: Vec<PatchOperation> = match serde_json::from_value(patch) {
        Ok(ops) => ops,
        Err(e) => panic!("appendix patch does not parse: {e}"),
    };
    let mut d = doc.clone();
    let mut budget = PatchBudget::for_document(&d, PatchLimits::default());
    match apply(&mut d, &ops, &mut budget) {
        Ok(()) => Ok(d),
        Err(e) => {
            assert_eq!(d, doc, "failed patch must leave the document unchanged");
            Err(e.reason)
        }
    }
}

#[test]
fn a01_add_object_member() {
    let out = run(
        json!({"foo": "bar"}),
        json!([{ "op": "add", "path": "/baz", "value": "qux" }]),
    );
    assert_eq!(out, Ok(json!({"baz": "qux", "foo": "bar"})));
}

#[test]
fn a02_add_array_element() {
    let out = run(
        json!({"foo": ["bar", "baz"]}),
        json!([{ "op": "add", "path": "/foo/1", "value": "qux" }]),
    );
    assert_eq!(out, Ok(json!({"foo": ["bar", "qux", "baz"]})));
}

#[test]
fn a03_remove_object_member() {
    let out = run(
        json!({"baz": "qux", "foo": "bar"}),
        json!([{ "op": "remove", "path": "/baz" }]),
    );
    assert_eq!(out, Ok(json!({"foo": "bar"})));
}

#[test]
fn a04_remove_array_element() {
    let out = run(
        json!({"foo": ["bar", "qux", "baz"]}),
        json!([{ "op": "remove", "path": "/foo/1" }]),
    );
    assert_eq!(out, Ok(json!({"foo": ["bar", "baz"]})));
}

#[test]
fn a05_replace_value() {
    let out = run(
        json!({"baz": "qux", "foo": "bar"}),
        json!([{ "op": "replace", "path": "/baz", "value": "boo" }]),
    );
    assert_eq!(out, Ok(json!({"baz": "boo", "foo": "bar"})));
}

#[test]
fn a06_move_value() {
    let out = run(
        json!({"foo": {"bar": "baz", "waldo": "fred"}, "qux": {"corge": "grault"}}),
        json!([{ "op": "move", "from": "/foo/waldo", "path": "/qux/thud" }]),
    );
    assert_eq!(
        out,
        Ok(json!({"foo": {"bar": "baz"}, "qux": {"corge": "grault", "thud": "fred"}}))
    );
}

#[test]
fn a07_move_array_element() {
    let out = run(
        json!({"foo": ["all", "grass", "cows", "eat"]}),
        json!([{ "op": "move", "from": "/foo/1", "path": "/foo/3" }]),
    );
    assert_eq!(out, Ok(json!({"foo": ["all", "cows", "eat", "grass"]})));
}

#[test]
fn a08_test_success() {
    let doc = json!({"baz": "qux", "foo": ["a", 2, "c"]});
    let out = run(
        doc.clone(),
        json!([
            { "op": "test", "path": "/baz", "value": "qux" },
            { "op": "test", "path": "/foo/1", "value": 2 }
        ]),
    );
    assert_eq!(out, Ok(doc));
}

#[test]
fn a09_test_error() {
    let out = run(
        json!({"baz": "qux"}),
        json!([{ "op": "test", "path": "/baz", "value": "bar" }]),
    );
    assert_eq!(out, Err(PatchErrorReason::TestFailed));
}

#[test]
fn a10_add_nested_member() {
    let out = run(
        json!({"foo": "bar"}),
        json!([{ "op": "add", "path": "/child", "value": { "grandchild": { } } }]),
    );
    assert_eq!(out, Ok(json!({"foo": "bar", "child": {"grandchild": {}}})));
}

#[test]
fn a11_ignore_unrecognized_elements() {
    let patch = json!([{ "op": "add", "path": "/baz", "value": "qux", "xyz": 123 }]);
    let out = run(json!({"foo": "bar"}), patch.clone());
    assert_eq!(out, Ok(json!({"foo": "bar", "baz": "qux"})));
    // ...and the unknown member survives re-serialisation.
    let ops: Vec<PatchOperation> = serde_json::from_value(patch.clone()).unwrap();
    assert_eq!(serde_json::to_value(&ops).unwrap(), patch);
}

#[test]
fn a12_add_to_nonexistent_target() {
    let out = run(
        json!({"foo": "bar"}),
        json!([{ "op": "add", "path": "/baz/bat", "value": "qux" }]),
    );
    assert_eq!(out, Err(PatchErrorReason::PathNotFound));
}

#[test]
fn a13_invalid_patch_document() {
    // `{"op":"add",...,"op":"remove"}`: serde_json keeps the last duplicate,
    // so this reaches the applier as `remove /baz`, which must not succeed.
    let patch: Value = serde_json::from_str(
        r#"[{ "op": "add", "path": "/baz", "value": "qux", "op": "remove" }]"#,
    )
    .unwrap();
    assert!(run(json!({"foo": "bar"}), patch).is_err());
}

#[test]
fn a14_tilde_escape_ordering() {
    let doc = json!({"/": 9, "~1": 10});
    let out = run(
        doc.clone(),
        json!([{"op": "test", "path": "/~01", "value": 10}]),
    );
    assert_eq!(out, Ok(doc));
}

#[test]
fn a15_comparing_strings_and_numbers() {
    let out = run(
        json!({"/": 9, "~1": 10}),
        json!([{"op": "test", "path": "/~01", "value": "10"}]),
    );
    assert_eq!(out, Err(PatchErrorReason::TestFailed));
}

#[test]
fn a16_add_array_value() {
    let out = run(
        json!({"foo": ["bar"]}),
        json!([{ "op": "add", "path": "/foo/-", "value": ["abc", "def"] }]),
    );
    assert_eq!(out, Ok(json!({"foo": ["bar", ["abc", "def"]]})));
}
