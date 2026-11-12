//! Unit tests for the patch engine: the RFC 6902 table, budgets, node
//! accounting and rollback.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use serde_json::{json, Value};

use super::*;

fn ops(v: Value) -> Vec<PatchOperation> {
    serde_json::from_value(v).unwrap()
}

fn count(v: &Value) -> usize {
    walk::count(v)
}

fn limits(max_depth: usize, growth_factor: usize, min_nodes: usize) -> PatchLimits {
    PatchLimits {
        max_depth,
        growth_factor,
        min_nodes,
        max_nodes: usize::MAX,
    }
}

/// Applies with default limits and checks atomicity and node accounting.
fn run(doc: &Value, patch: &[PatchOperation]) -> Result<Value, PatchError> {
    let mut d = doc.clone();
    let mut budget = PatchBudget::for_document(&d, PatchLimits::default());
    let r = apply(&mut d, patch, &mut budget);
    match r {
        Ok(()) => {
            assert_eq!(budget.nodes(), count(&d), "node count drifted");
            Ok(d)
        }
        Err(e) => {
            assert_eq!(&d, doc, "document changed by a failed patch");
            assert_eq!(
                budget.nodes(),
                count(doc),
                "budget changed by a failed patch"
            );
            Err(e)
        }
    }
}

/// The component's RFC 6902/6901 case table. Structural (`Malformed*`)
/// cases (enforcement's job, SNAP-09) either fail to deserialize or, when
/// the typed form can carry them (an absent or `null` operand), fail with
/// `MissingMember`.
#[test]
fn json_cases() {
    let cases: Vec<Value> = serde_json::from_str(include_str!("cases.json")).unwrap();
    assert!(cases.len() > 80);
    for case in &cases {
        let name = case["comment"].as_str().unwrap();
        let parsed: Result<Vec<PatchOperation>, _> = serde_json::from_value(case["patch"].clone());
        let want_error = case.get("error").and_then(Value::as_str);
        let expected_reason = match want_error {
            None => None,
            Some("IndexOutOfRange" | "InvalidIndex") => Some(PatchErrorReason::InvalidIndex),
            Some("PathMissing" | "TypeMismatch" | "RemoveRoot" | "InvalidPointer") => {
                Some(PatchErrorReason::PathNotFound)
            }
            Some("TestFailed") => Some(PatchErrorReason::TestFailed),
            Some("MoveIntoChild") => Some(PatchErrorReason::MoveIntoChild),
            // Structural defects: either the typed path cannot carry them,
            // or (an absent or `null` operand) apply reports the gap.
            Some(m) if m.starts_with("Malformed") => {
                if parsed.is_err() {
                    continue;
                }
                Some(PatchErrorReason::MissingMember)
            }
            Some(other) => panic!("{name}: unmapped error {other}"),
        };
        let patch = parsed.unwrap_or_else(|e| panic!("{name}: {e}"));
        match (run(&case["doc"], &patch), expected_reason) {
            (Ok(got), None) => assert_eq!(&got, &case["expected"], "{name}"),
            (Err(e), Some(reason)) => assert_eq!(e.reason, reason, "{name}: {e}"),
            (got, want) => panic!("{name}: got {got:?}, want {want:?}"),
        }
    }
}

#[test]
fn present_null_value_is_some_and_absent_is_none() {
    let p =
        ops(json!([{"op": "add", "path": "/a", "value": null}, {"op": "remove", "path": "/a"}]));
    assert_eq!(p[0].value, Some(Value::Null));
    assert_eq!(p[1].value, None);
    assert_eq!(run(&json!({}), &p[..1]).unwrap(), json!({"a": null}));
}

#[test]
fn from_is_typed_only_where_rfc_6902_defines_it() {
    // `move`/`copy`: a pointer string, `null` reads as absent, any other type
    // is malformed, wherever the member sits.
    assert!(serde_json::from_value::<PatchOperation>(
        json!({"op": "move", "from": 5, "path": "/a"})
    )
    .is_err());
    assert!(serde_json::from_value::<PatchOperation>(
        json!({"op": "copy", "path": "/a", "from": {}})
    )
    .is_err());
    let p = ops(json!([
        {"op": "move", "path": "/b", "from": "/a"},
        {"op": "copy", "path": "/b", "from": null},
        {"op": "remove", "path": "/a", "from": "/x"},
        {"op": "remove", "path": "/a", "from": 5},
        {"op": "remove", "path": "/a", "from": null},
    ]));
    assert_eq!(p[0].from.as_ref().map(JsonPointer::as_str), Some("/a"));
    assert_eq!(p[1].from, None);
    // Elsewhere a string stays in the field and anything else is an
    // extension member; the two never both hold `from`.
    assert_eq!(p[2].from.as_ref().map(JsonPointer::as_str), Some("/x"));
    assert!(p[2].extensions.is_empty());
    assert_eq!(
        (p[3].from.as_ref(), p[3].extensions.get("from")),
        (None, Some(&json!(5)))
    );
    assert_eq!(
        (p[4].from.as_ref(), p[4].extensions.get("from")),
        (None, Some(&Value::Null))
    );
    for op in &p[2..] {
        let text = serde_json::to_string(op).unwrap();
        assert_eq!(text.matches("\"from\"").count(), 1, "{text}");
    }
}

#[test]
fn extension_members_survive_and_are_ignored() {
    let raw = json!([{"op": "replace", "path": "/a", "value": 2, "x-trace": {"id": 7}}]);
    let p = ops(raw.clone());
    assert_eq!(p[0].extensions.get("x-trace"), Some(&json!({"id": 7})));
    assert_eq!(serde_json::to_value(&p).unwrap(), raw);
    assert_eq!(run(&json!({"a": 1}), &p).unwrap(), json!({"a": 2}));
}

#[test]
fn error_names_the_failing_index_without_values() {
    let doc = json!({"secret": "hunter2", "a": [1]});
    let p = ops(json!([
        {"op": "test", "path": "/a/0", "value": 1},
        {"op": "replace", "path": "/a/0", "value": 5},
        {"op": "test", "path": "/secret", "value": "nope"}
    ]));
    let e = run(&doc, &p).unwrap_err();
    assert_eq!(e.index, 2);
    assert_eq!(e.reason, PatchErrorReason::TestFailed);
    let msg = e.to_string();
    assert_eq!(
        msg,
        "JSON Patch operation 2 not applied: test operation failed"
    );
    assert!(!msg.contains("hunter2") && !msg.contains("nope"));
}

#[test]
fn missing_members_and_bad_pointers_fail_cleanly() {
    let doc = json!({"a": 1});
    let mut add = PatchOperation::add(JsonPointer::new_unchecked("/b"), json!(1));
    add.value = None;
    assert_eq!(
        run(&doc, &[add]).unwrap_err().reason,
        PatchErrorReason::MissingMember
    );
    let mut mv = PatchOperation::move_to(
        JsonPointer::new_unchecked("/a"),
        JsonPointer::new_unchecked("/b"),
    );
    mv.from = None;
    assert_eq!(
        run(&doc, &[mv]).unwrap_err().reason,
        PatchErrorReason::MissingMember
    );
    let bad = PatchOperation::remove(JsonPointer::new_unchecked("a"));
    assert_eq!(
        run(&doc, &[bad]).unwrap_err().reason,
        PatchErrorReason::PathNotFound
    );
}

#[test]
fn depth_limit_on_add_and_move() {
    fn nested(depth: usize) -> Value {
        let mut v = json!(0);
        for _ in 0..depth {
            v = json!({ "n": v });
        }
        v
    }
    let lim = limits(4, 4, 1_000);
    let mut d = json!({"a": {}, "s": {"t": {"u": 1}}});
    let mut b = PatchBudget::for_document(&d, lim);
    // /a/x is depth 2; a value of height 2 reaches 4: allowed.
    apply(
        &mut d,
        &ops(json!([{"op": "add", "path": "/a/x", "value": nested(2)}])),
        &mut b,
    )
    .unwrap();
    let e = apply(
        &mut d,
        &ops(json!([{"op": "add", "path": "/a/y", "value": nested(3)}])),
        &mut b,
    )
    .unwrap_err();
    assert_eq!((e.index, e.reason), (0, PatchErrorReason::TooDeep));
    // Moving /s (height 2) to /a/x/n/s (depth 4) would reach 6.
    let e = apply(
        &mut d,
        &ops(json!([{"op": "move", "from": "/s", "path": "/a/x/n/s"}])),
        &mut b,
    )
    .unwrap_err();
    assert_eq!(e.reason, PatchErrorReason::TooDeep);
    let e = apply(
        &mut d,
        &ops(json!([{"op": "add", "path": "/1/2/3/4/5", "value": 1}])),
        &mut b,
    )
    .unwrap_err();
    assert_eq!(e.reason, PatchErrorReason::TooDeep);
    assert_eq!(b.nodes(), count(&d));
}

#[test]
fn copy_bomb_is_refused_cheaply_and_rolled_back() {
    // 100k nodes; budget max(1M, 4 x 100k) = 1M, under a ceiling raised
    // above it so the growth allowance is what binds.
    let doc = Value::Array((0..99_999).map(|i| json!(i)).collect());
    let mut d = doc.clone();
    let limits = PatchLimits {
        max_nodes: 4_000_000,
        ..PatchLimits::default()
    };
    let mut b = PatchBudget::for_document(&d, limits);
    assert_eq!(b.nodes(), 100_000);
    let bomb: Vec<Value> = (0..10_000)
        .map(|i| json!({"op": "copy", "from": "", "path": format!("/{i}")}))
        .collect();
    let started = std::time::Instant::now();
    let e = apply(&mut d, &ops(Value::Array(bomb)), &mut b).unwrap_err();
    assert!(started.elapsed() < std::time::Duration::from_secs(5));
    // 100k -> 200k -> 400k -> 800k; the copy at index 3 would make 1.6M.
    assert_eq!((e.index, e.reason), (3, PatchErrorReason::BudgetExceeded));
    assert!(d == doc);
    assert_eq!(b.nodes(), 100_000);
}

/// The growth allowance is relative to the document (docs/DECISIONS.md D14),
/// so without an absolute ceiling every small patch could double it again.
#[test]
fn growth_never_compounds_past_the_absolute_ceiling() {
    let mut d = json!({"s": 1});
    let lim = PatchLimits {
        max_nodes: 10_000,
        ..PatchLimits::default()
    };
    let mut b = PatchBudget::for_document(&d, lim);
    let mut refused = None;
    for k in 0..64 {
        let op = ops(json!([{"op": "copy", "from": "", "path": format!("/k{k}")}]));
        match apply(&mut d, &op, &mut b) {
            Ok(()) => assert!(b.nodes() <= 10_000),
            Err(e) => {
                refused = Some((k, e.reason));
                break;
            }
        }
    }
    // 2 nodes doubling: 2^13 = 8192 fits, 2^14 does not.
    assert_eq!(refused, Some((12, PatchErrorReason::NodeCeilingExceeded)));
    assert_eq!(b.nodes(), 8192);
    assert_eq!(b.nodes(), count(&d));
}

#[test]
fn node_budget_tracks_growth_and_frees_on_remove() {
    let mut d = json!({});
    let mut b = PatchBudget::for_document(&d, limits(128, 1, 10));
    apply(
        &mut d,
        &ops(json!([{"op": "add", "path": "/a", "value": [1, 2, 3, 4, 5, 6, 7, 8]}])),
        &mut b,
    )
    .unwrap();
    assert_eq!(b.nodes(), 10);
    let e = apply(
        &mut d,
        &ops(json!([{"op": "add", "path": "/b", "value": 1}])),
        &mut b,
    )
    .unwrap_err();
    assert_eq!(e.reason, PatchErrorReason::BudgetExceeded);
    apply(
        &mut d,
        &ops(json!([
            {"op": "replace", "path": "/a", "value": 0},
            {"op": "add", "path": "/b", "value": [1, 2, 3, 4, 5, 6, 7]}
        ])),
        &mut b,
    )
    .unwrap();
    assert_eq!(b.nodes(), 10);
    assert_eq!(b.nodes(), count(&d));
}

#[test]
fn node_count_tracks_exactly() {
    let mut d = json!({"a": [1, {"b": 2}], "c": "x"});
    let mut b = PatchBudget::for_document(&d, PatchLimits::default());
    let steps = [
        json!([{"op": "add", "path": "/d", "value": {"e": [1, 2, 3]}}]),
        json!([{"op": "copy", "from": "/a", "path": "/d/e/1"}]),
        json!([{"op": "move", "from": "/a/1", "path": "/c"}]),
        json!([{"op": "replace", "path": "/d", "value": [[], {}]}]),
        json!([{"op": "remove", "path": "/a"}]),
        json!([{"op": "move", "from": "/d", "path": ""}]),
        json!([{"op": "copy", "from": "", "path": "/-"}]),
        json!([{"op": "add", "path": "", "value": {"z": [null]}}]),
    ];
    for s in steps {
        apply(&mut d, &ops(s.clone()), &mut b).unwrap_or_else(|e| panic!("{s}: {e}"));
        assert_eq!(b.nodes(), count(&d), "after {s}");
    }
}

#[test]
fn failed_patch_is_byte_identical_for_every_prefix() {
    let doc = json!({
        "z": 1, "a": {"k3": 3, "k1": 1, "k2": 2},
        "arr": [0, 1, 2, 3, 4, {"deep": [1, 2]}],
        "m": {"x~y": {"a/b": true}}
    });
    let before = serde_json::to_string(&doc).unwrap();
    let p = ops(json!([
        {"op": "remove", "path": "/a/k1"},
        {"op": "add", "path": "/a/k0", "value": 0},
        {"op": "add", "path": "/a/k2", "value": "over"},
        {"op": "move", "from": "/arr/0", "path": "/arr/-"},
        {"op": "move", "from": "/arr/4/deep", "path": "/deep"},
        {"op": "move", "from": "/z", "path": "/a/k3"},
        {"op": "copy", "from": "/m", "path": "/arr/0"},
        {"op": "replace", "path": "/m/x~0y/a~1b", "value": false},
        {"op": "remove", "path": "/arr/2"},
        {"op": "move", "from": "/a", "path": ""},
        {"op": "add", "path": "/k9", "value": [1]},
        {"op": "move", "from": "/k2", "path": "/k9/0"},
        {"op": "test", "path": "/k9/0", "value": "nope"}
    ]));
    let e = run(&doc, &p).unwrap_err();
    assert_eq!((e.index, e.reason), (12, PatchErrorReason::TestFailed));
    for k in 1..p.len() {
        let mut prefix = p[..k].to_vec();
        prefix.push(PatchOperation::test(
            JsonPointer::new_unchecked(""),
            json!("never"),
        ));
        let mut v = doc.clone();
        let mut b = PatchBudget::for_document(&v, PatchLimits::default());
        apply(&mut v, &prefix, &mut b).unwrap_err();
        assert_eq!(serde_json::to_string(&v).unwrap(), before, "prefix {k}");
        assert_eq!(b.nodes(), count(&doc), "prefix {k}");
    }
}

/// Deterministic pseudo-random patches: every outcome is either a success
/// with an exact node count or a failure that left the document identical.
#[test]
fn random_patches_are_atomic_and_counted() {
    let mut seed: u64 = 0x9e37_79b9_7f4a_7c15;
    let mut next = move |n: usize| {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        (seed % n as u64) as usize
    };
    let paths = [
        "", "/a", "/a/0", "/a/-", "/a/1", "/b", "/b/c", "/b/c/0", "/x", "/a/9",
    ];
    let kinds = ["add", "remove", "replace", "move", "copy", "test"];
    let values = [
        json!(1),
        json!([1, 2]),
        json!({"c": [0]}),
        json!(null),
        json!({}),
    ];
    let mut doc = json!({"a": [1, 2, 3], "b": {"c": [4]}});
    let mut budget = PatchBudget::for_document(&doc, limits(6, 2, 64));
    for _ in 0..4_000 {
        let n = 1 + next(4);
        let mut list = Vec::new();
        for _ in 0..n {
            let mut op = serde_json::Map::new();
            op.insert("op".into(), json!(kinds[next(kinds.len())]));
            op.insert("path".into(), json!(paths[next(paths.len())]));
            op.insert("from".into(), json!(paths[next(paths.len())]));
            op.insert("value".into(), values[next(values.len())].clone());
            list.push(Value::Object(op));
        }
        let p = ops(Value::Array(list));
        let before = doc.clone();
        match apply(&mut doc, &p, &mut budget) {
            Ok(()) => assert_eq!(budget.nodes(), count(&doc)),
            Err(_) => {
                assert_eq!(doc, before);
                assert_eq!(
                    serde_json::to_string(&doc).unwrap(),
                    serde_json::to_string(&before).unwrap()
                );
                assert_eq!(budget.nodes(), count(&doc));
            }
        }
        if !doc.is_object() || budget.nodes() > 40 {
            doc = json!({"a": [1, 2, 3], "b": {"c": [4]}});
            budget = PatchBudget::for_document(&doc, limits(6, 2, 64));
        }
    }
}

#[test]
fn checked_apply_refuses_and_restores() {
    let mut d = json!({"a": 1});
    let mut b = PatchBudget::for_document(&d, PatchLimits::default());
    let p = ops(
        json!([{"op": "add", "path": "/b", "value": 2}, {"op": "replace", "path": "", "value": [1]}]),
    );
    let r = apply_checked(&mut d, &p, &mut b, usize::MAX, Value::is_object);
    assert_eq!(r, Err(CheckedError::Refused));
    assert_eq!(d, json!({"a": 1}));
    assert_eq!(b.nodes(), 2);
}

#[test]
fn pointer_parse_and_tokens() {
    assert!(JsonPointer::parse("/a~1b/~0c/").is_ok());
    assert!(JsonPointer::parse("a").is_err());
    assert!(JsonPointer::parse("/~2").is_err());
    let p = JsonPointer::parse("/a~1b/~0c/").unwrap();
    let toks: Vec<_> = p.tokens().collect();
    assert_eq!(toks, ["a/b", "~c", ""]);
    assert!(matches!(
        p.tokens().next(),
        Some(std::borrow::Cow::Owned(_))
    ));
    let plain = JsonPointer::parse("/plain").unwrap();
    assert!(matches!(
        plain.tokens().next(),
        Some(std::borrow::Cow::Borrowed("plain"))
    ));
    assert_eq!(JsonPointer::parse("").unwrap().tokens().count(), 0);
    assert_eq!(
        JsonPointer::parse("/")
            .unwrap()
            .tokens()
            .collect::<Vec<_>>(),
        [""]
    );
}

#[test]
fn constructors_serialize_to_rfc_shapes() {
    let p = |s: &str| JsonPointer::new_unchecked(s);
    let list = vec![
        PatchOperation::add(p("/a"), json!(1)),
        PatchOperation::remove(p("/a")),
        PatchOperation::replace(p("/a"), json!(null)),
        PatchOperation::move_to(p("/a"), p("/b")),
        PatchOperation::copy_to(p("/a"), p("/b")),
        PatchOperation::test(p("/a"), json!([])),
    ];
    assert_eq!(
        serde_json::to_value(&list).unwrap(),
        json!([
            {"op": "add", "path": "/a", "value": 1},
            {"op": "remove", "path": "/a"},
            {"op": "replace", "path": "/a", "value": null},
            {"op": "move", "path": "/b", "from": "/a"},
            {"op": "copy", "path": "/b", "from": "/a"},
            {"op": "test", "path": "/a", "value": []}
        ])
    );
}

#[test]
fn diff_round_trips() {
    let cases = [
        (
            json!({"a": 1, "b": [1, 2], "c": {"d": "x", "e": 1}}),
            json!({"a": 2, "b": [1, 2], "c": {"d": "x", "f": null}, "g~/": true}),
        ),
        (json!({"a": [1, 2]}), json!({"a": [2, 1]})),
        (json!(1), json!({"a": 1})),
        (json!({"a": {}}), json!({"a": {}})),
        (json!(null), json!(false)),
    ];
    for (before, after) in cases {
        let patch = diff(&before, &after);
        let mut v = before.clone();
        let mut b = PatchBudget::for_document(&v, PatchLimits::default());
        apply(&mut v, &patch, &mut b).unwrap();
        assert_eq!(v, after, "{before} -> {after}");
    }
    assert!(diff(&json!({"a": [1]}), &json!({"a": [1]})).is_empty());
    assert_eq!(
        serde_json::to_value(diff(&json!({"a": 1, "b": [1]}), &json!({"b": [2], "c": 3}))).unwrap(),
        json!([
            {"op": "remove", "path": "/a"},
            {"op": "replace", "path": "/b", "value": [2]},
            {"op": "add", "path": "/c", "value": 3}
        ])
    );
}

/// An `op` RFC 6902 does not define never reaches the typed path: enforcement
/// drops that element with a warning (SNAP-10) before deserialization.
#[test]
fn unknown_op_does_not_deserialize() {
    let r: Result<PatchOperation, _> =
        serde_json::from_value(json!({"op": "frobnicate", "path": "/a"}));
    assert!(r.is_err());
}

/// Rollback restores member order too: byte-identical output under
/// `preserve-order` (insertion order) as well as with sorted maps.
#[test]
fn rollback_restores_member_order() {
    let doc: Value =
        serde_json::from_str(r#"{"z": 1, "a": {"y": 1, "b": 2, "x": 3}, "m": [1]}"#).unwrap();
    let before = serde_json::to_string(&doc).unwrap();
    let p = ops(json!([
        {"op": "remove", "path": "/a/y"},
        {"op": "remove", "path": "/z"},
        {"op": "add", "path": "/a/new", "value": 0},
        {"op": "move", "from": "/a/b", "path": "/q"},
        {"op": "move", "from": "/m", "path": "/a/x"},
        {"op": "test", "path": "/q", "value": "never"}
    ]));
    let e = run(&doc, &p).unwrap_err();
    assert_eq!(e.reason, PatchErrorReason::TestFailed);
    let mut v = doc.clone();
    let mut b = PatchBudget::for_document(&v, PatchLimits::default());
    apply(&mut v, &p, &mut b).unwrap_err();
    assert_eq!(serde_json::to_string(&v).unwrap(), before);
    // A successful removal keeps the remaining members in order.
    let p = ops(json!([{"op": "remove", "path": "/a/y"}]));
    let got = serde_json::to_string(&run(&doc, &p).unwrap()).unwrap();
    let want: Value = serde_json::from_str(r#"{"z": 1, "a": {"b": 2, "x": 3}, "m": [1]}"#).unwrap();
    assert_eq!(got, serde_json::to_string(&want).unwrap());
}

/// Positional shifts are work: one walk unit per 16 elements moved, so a
/// patch of head removals/insertions on a big array cannot be quadratic.
#[test]
fn array_shifts_are_charged() {
    const N: usize = 100_000;
    let doc = json!({"a": vec![0; N]});
    let mut pairs = Vec::new();
    for _ in 0..5_000 {
        pairs.push(json!({"op": "remove", "path": "/a/0"}));
        pairs.push(json!({"op": "add", "path": "/a/0", "value": 0}));
    }
    let e = run(&doc, &ops(Value::Array(pairs))).unwrap_err();
    assert_eq!(e.reason, PatchErrorReason::BudgetExceeded);
    // walk_cap = 4 x 1M; each op moves N elements = N / 16 units.
    let walk_cap = 4_000_000;
    assert!(
        e.index <= 2 * walk_cap * 16 / N + 4,
        "refused too late: {}",
        e.index
    );
}

#[test]
fn legitimate_head_removals_still_apply() {
    let doc = json!({"a": vec![0; 100_000]});
    let p: Vec<Value> = (0..100)
        .map(|_| json!({"op": "remove", "path": "/a/0"}))
        .collect();
    let out = run(&doc, &ops(Value::Array(p))).unwrap();
    assert_eq!(out["a"].as_array().unwrap().len(), 99_900);
}

/// The node ceiling bounds memory, not work (docs/DECISIONS.md D14): under the default 180,000-unit ceiling a patch still gets the
/// 4,000,000-unit walk budget of its growth allowance, so 200 head removals
/// on a 100,000-element array (about 1.25M units of shifts) apply, as they
/// did with the old 4,000,000-unit ceiling.
#[test]
fn the_ceiling_does_not_shrink_the_walk_budget() {
    let doc = json!({"a": vec![0; 100_000]});
    let p: Vec<Value> = (0..200)
        .map(|_| json!({"op": "remove", "path": "/a/0"}))
        .collect();
    let mut d = doc.clone();
    let mut b = PatchBudget::for_document(&d, PatchLimits::default());
    apply(&mut d, &ops(Value::Array(p)), &mut b).unwrap();
    assert_eq!(d["a"].as_array().unwrap().len(), 99_800);
}

#[cfg(feature = "preserve-order")]
fn wide_object(n: usize) -> Value {
    Value::Object((0..n).map(|i| (format!("k{i}"), json!(0))).collect())
}

/// Under `preserve-order` removal is O(members); it is charged too.
#[cfg(feature = "preserve-order")]
#[test]
fn object_removals_are_charged() {
    const N: usize = 50_000;
    let doc = wide_object(N);
    let p: Vec<Value> = (0..3_000)
        .map(|i| json!({"op": "remove", "path": format!("/k{i}")}))
        .collect();
    let e = run(&doc, &ops(Value::Array(p))).unwrap_err();
    assert_eq!(e.reason, PatchErrorReason::BudgetExceeded);
    assert!(e.index <= 2 * 4_000_000 * 16 / N + 4, "late: {}", e.index);
}

/// `diff` charges an object's member removals before it descends into the
/// members it keeps, so once they alone pass the walk cap it stops there
/// instead of walking (and probing) every deeper level first.
#[cfg(feature = "preserve-order")]
#[test]
fn diff_charges_removals_before_descending() {
    const M: usize = 400;
    let mut before = serde_json::Map::new();
    // `a` is too big for rebuilding the object to be cheaper.
    let pad = vec![0; 2 * M * M];
    before.insert("a".into(), json!({"x": 1, "pad": pad}));
    for i in 0..M {
        before.insert(format!("k{i}"), json!(0));
    }
    let before = Value::Object(before);
    let after = json!({"a": {"x": 2, "pad": pad}});
    let shifts = M * engine::remove_shift(M + 1);
    let mut cost = Cost {
        walk: 0,
        limit: shifts - 1,
        added: 0,
        freed: 0,
        headroom: usize::MAX,
        over_ceiling: false,
        grew_past: false,
        path_bytes: 0,
        path_check: PATH_CHECK_FLOOR,
        paths_too_big: false,
        after: &after,
    };
    let mut out = Vec::new();
    diff_into(&before, &after, &mut String::new(), &mut out, &mut cost);
    assert!(cost.over());
    assert!(
        out.iter().all(|op| op.path.as_str() != "/a/x"),
        "descended into /a: {out:?}"
    );
}

/// A missing key is `PathNotFound` however much budget is left.
#[cfg(feature = "preserve-order")]
#[test]
fn missing_key_removal_reports_path_not_found_when_budget_is_spent() {
    const N: usize = 50_000;
    let mut d = wide_object(N);
    let mut b = PatchBudget::for_document(&d, limits(64, 1, 1));
    // walk_cap = 4 x 50_001; each removal costs about 1 + N / 4 = 12,501.
    let mut p: Vec<Value> = (0..16)
        .map(|i| json!({"op": "remove", "path": format!("/k{i}")}))
        .collect();
    p.push(json!({"op": "remove", "path": "/missing"}));
    let e = apply(&mut d, &ops(Value::Array(p)), &mut b).unwrap_err();
    assert_eq!((e.index, e.reason), (16, PatchErrorReason::PathNotFound));
}

/// Counting a displaced subtree is work: it is charged to the walk budget
/// like the clone that made it, so copy/remove churn is bounded.
#[test]
fn displaced_subtree_counts_are_charged() {
    let doc = json!({"a": vec![0; 1000]});
    let mut d = doc.clone();
    // 1002 nodes: node budget 2004, walk budget 8016.
    let mut b = PatchBudget::for_document(&d, limits(64, 2, 1));
    let mut p = Vec::new();
    for _ in 0..20 {
        p.push(json!({"op": "copy", "from": "/a", "path": "/c"}));
        p.push(json!({"op": "remove", "path": "/c"}));
    }
    let e = apply(&mut d, &ops(Value::Array(p)), &mut b).unwrap_err();
    assert_eq!(e.reason, PatchErrorReason::BudgetExceeded);
    // Counts are deferred until the node budget binds, then charged: the
    // churn stops at index 8. Uncharged counts reached index 14.
    assert_eq!(e.index, 8);
    assert!(d == doc);
}

/// A failing frame does not pay for the subtree an earlier operation removed:
/// the count is deferred until the node budget would bind.
#[test]
fn removed_subtree_is_not_counted_by_a_frame_that_fails() {
    let doc = json!({"big": vec![0; 1000], "s": vec![0; 1000]});
    let mut d = doc.clone();
    // 2003 nodes, walk budget 8012. Counting /big costs 1001; each test of
    // /s costs 1002, so seven tests exhaust the budget only if it was counted.
    let mut b = PatchBudget::for_document(&d, limits(64, 1, 1));
    let mut p = vec![json!({"op": "remove", "path": "/big"})];
    for _ in 0..7 {
        p.push(json!({"op": "test", "path": "/s", "value": vec![0; 1000]}));
    }
    p.push(json!({"op": "test", "path": "/s", "value": 1}));
    let e = apply(&mut d, &ops(Value::Array(p)), &mut b).unwrap_err();
    assert_eq!((e.index, e.reason), (8, PatchErrorReason::TestFailed));
    assert!(d == doc);
}

/// Deferred credit is still given when the node budget binds.
#[test]
fn removal_credit_is_settled_when_the_node_budget_binds() {
    let doc = json!({"big": vec![0; 1000]});
    let mut d = doc.clone();
    let mut b = PatchBudget::for_document(&d, limits(64, 1, 1));
    let p = json!([
        {"op": "remove", "path": "/big"},
        {"op": "add", "path": "/new", "value": vec![0; 1000]}
    ]);
    apply(&mut d, &ops(p), &mut b).unwrap();
    assert_eq!(count(&d), 1002);
    assert_eq!(b.nodes, 1002);
}

/// A copy from a missing source fails on the source, not on the cost of
/// counting what it would have displaced.
#[test]
fn copy_from_missing_source_fails_before_counting_the_target() {
    let doc = json!({"big": vec![0; 1000], "s": vec![0; 20]});
    let mut d = doc.clone();
    // 1023 nodes, walk budget 4092; 4 x test(/big) leave under a thousand units.
    let mut b = PatchBudget::for_document(&d, limits(64, 1, 1));
    let mut p = Vec::new();
    for _ in 0..4 {
        p.push(json!({"op": "test", "path": "/big", "value": vec![0; 1000]}));
    }
    p.push(json!({"op": "copy", "from": "/missing", "path": "/big"}));
    let e = apply(&mut d, &ops(Value::Array(p)), &mut b).unwrap_err();
    assert_eq!((e.index, e.reason), (4, PatchErrorReason::PathNotFound));
}

/// A copy over an existing member counts what it displaces once.
#[test]
fn copy_over_a_member_charges_the_displaced_count_once() {
    let doc = json!({"big": vec![0; 1000], "t": vec![0; 1000]});
    let mut d = doc.clone();
    // 2003 nodes, walk budget 8012. Each copy costs its clone (1001) plus the
    // count of the 1001 nodes it displaces: 2002 once, 3003 if counted twice.
    let mut b = PatchBudget::for_document(&d, limits(64, 1, 1));
    let p: Vec<Value> = (0..3)
        .map(|_| json!({"op": "copy", "from": "/big", "path": "/t"}))
        .collect();
    // 3 x 2002 + depth charges fits 8012; counted twice, 3 x 3003 does not.
    apply(&mut d, &ops(Value::Array(p)), &mut b).unwrap();
}

/// Counting a displaced value stops at the limit and, for a wide container,
/// before it queues the container's members.
#[test]
fn count_displaced_is_capped() {
    let wide = Value::Array(vec![json!(0); 1_000_000]);
    assert_eq!(walk::count_displaced(&wide, 10), None);
    assert_eq!(walk::count_displaced(&wide, 1_000_001), Some(1_000_001));
    assert_eq!(walk::count_displaced(&wide, 1_000_000), None);
    assert_eq!(
        walk::count_displaced(&json!({"a": [1, {"b": 2}]}), 5),
        Some(5)
    );
    assert_eq!(walk::count_displaced(&json!({"a": [1, {"b": 2}]}), 4), None);
    // Deeper than the recursive range.
    let mut deep = json!(0);
    for _ in 0..600 {
        deep = json!([deep, 1]);
    }
    assert_eq!(walk::count_displaced(&deep, usize::MAX), Some(1 + 600 * 2));
    assert_eq!(walk::count_displaced(&deep, 1200), None);
}

/// The crate never emits a delta its own default limits refuse: deleting many
/// members of a very wide object is sent as one `replace`.
#[cfg(feature = "preserve-order")]
#[test]
fn diff_of_many_deletions_from_a_wide_object_is_applicable() {
    let before = wide_object(100_000);
    let mut after = before.clone();
    for i in 0..1000 {
        after.as_object_mut().unwrap().remove(&format!("k{i}"));
    }
    let patch = diff(&before, &after);
    assert!(patch.len() < 1000, "{} operations", patch.len());
    assert_eq!(run(&before, &patch).unwrap(), after);
}

/// A few deletions stay member-wise.
#[cfg(feature = "preserve-order")]
#[test]
fn diff_of_few_deletions_stays_member_wise() {
    let before = wide_object(100_000);
    let mut after = before.clone();
    after.as_object_mut().unwrap().remove("k5");
    assert_eq!(diff(&before, &after).len(), 1);
}

fn pinned(doc: &Value) -> PatchBudget {
    PatchBudget::for_document(
        doc,
        PatchLimits {
            max_depth: 64,
            growth_factor: 1,
            min_nodes: 1,
            max_nodes: count(doc),
        },
    )
}

/// At the absolute node ceiling a failing frame still does not pay for the
/// subtree it removed: only the deficit is counted.
#[test]
fn failing_frame_at_the_ceiling_counts_only_the_deficit() {
    let doc = json!({"big": vec![0; 1000], "s": vec![0; 1000]});
    let mut d = doc.clone();
    let mut b = pinned(&d);
    let mut p = vec![
        json!({"op": "remove", "path": "/big"}),
        json!({"op": "add", "path": "/z", "value": 1}),
    ];
    for _ in 0..7 {
        p.push(json!({"op": "test", "path": "/s", "value": vec![0; 1000]}));
    }
    p.push(json!({"op": "test", "path": "/s", "value": 1}));
    let e = apply(&mut d, &ops(Value::Array(p)), &mut b).unwrap_err();
    assert_eq!((e.index, e.reason), (9, PatchErrorReason::TestFailed));
    assert!(d == doc);
}

/// A partial credit is resumed when later additions need more of the same
/// subtree, and the final count is exact.
#[test]
fn partial_credit_is_extended_and_final_count_exact() {
    let doc = json!({"big": vec![0; 100_000], "s": 1});
    let mut d = doc.clone();
    let mut b = pinned(&d);
    let p = json!([
        {"op": "remove", "path": "/big"},
        {"op": "add", "path": "/a", "value": vec![0; 50_000]},
        {"op": "add", "path": "/b", "value": vec![0; 40_000]}
    ]);
    apply(&mut d, &ops(p), &mut b).unwrap();
    assert_eq!(b.nodes(), count(&d));
}

/// A refused patch does not count what it displaced.
#[test]
fn refused_patch_does_not_count_displaced_content() {
    let doc = json!({"big": vec![0; 1000], "s": vec![0; 1000]});
    let mut d = doc.clone();
    let mut b = PatchBudget::for_document(&d, limits(64, 1, 1));
    let mut p = Vec::new();
    for _ in 0..7 {
        p.push(json!({"op": "test", "path": "/s", "value": vec![0; 1000]}));
    }
    p.push(json!({"op": "replace", "path": "", "value": 1}));
    let r = apply_checked(
        &mut d,
        &ops(Value::Array(p)),
        &mut b,
        usize::MAX,
        Value::is_object,
    );
    assert_eq!(r, Err(CheckedError::Refused));
    assert!(d == doc);
}

/// At the ceiling a failing frame whose copy needs headroom pays for the
/// deficit, not for the subtree an earlier operation removed.
#[test]
fn failing_frame_with_a_copy_at_the_ceiling_counts_only_the_deficit() {
    let doc = json!({"big": vec![0; 1000], "s": vec![0; 1000]});
    let mut d = doc.clone();
    let mut b = pinned(&d);
    let mut p = vec![
        json!({"op": "remove", "path": "/big"}),
        json!({"op": "copy", "from": "/s/0", "path": "/z"}),
    ];
    for _ in 0..7 {
        p.push(json!({"op": "test", "path": "/s", "value": vec![0; 1000]}));
    }
    p.push(json!({"op": "test", "path": "/s", "value": 1}));
    let e = apply(&mut d, &ops(Value::Array(p)), &mut b).unwrap_err();
    assert_eq!((e.index, e.reason), (9, PatchErrorReason::TestFailed));
    assert!(d == doc);
}

/// At the ceiling a copy over an existing member counts only as much of the
/// member as the copy needs, so a failing frame does not pay for all of it.
#[test]
fn failing_frame_copying_over_a_member_at_the_ceiling_counts_only_the_deficit() {
    let doc = json!({"big": vec![0; 1000], "s": vec![0; 1000]});
    let mut d = doc.clone();
    let mut b = pinned(&d);
    let mut p = vec![
        json!({"op": "copy", "from": "/s/0", "path": "/big"}),
        json!({"op": "test", "path": "/s/0", "value": 0}),
    ];
    for _ in 0..7 {
        p.push(json!({"op": "test", "path": "/s", "value": vec![0; 1000]}));
    }
    p.push(json!({"op": "test", "path": "/s", "value": 1}));
    let e = apply(&mut d, &ops(Value::Array(p)), &mut b).unwrap_err();
    assert_eq!((e.index, e.reason), (9, PatchErrorReason::TestFailed));
    assert!(d == doc);
}

/// A copy over a member partially counted at the ceiling still leaves an
/// exact node count once the patch commits.
#[test]
fn copy_over_a_partially_counted_member_leaves_an_exact_count() {
    let doc = json!({"big": vec![0; 1000], "s": vec![0; 10], "t": 1});
    let mut d = doc.clone();
    let mut b = pinned(&d);
    let p = json!([
        {"op": "copy", "from": "/s", "path": "/big"},
        {"op": "add", "path": "/u", "value": vec![0; 500]},
        {"op": "copy", "from": "/s", "path": "/t"}
    ]);
    apply(&mut d, &ops(p), &mut b).unwrap();
    assert_eq!(b.nodes(), count(&d));
}

/// A document already past the ceiling (a snapshot is ingested unchecked)
/// can still shrink: counting what is resident is always affordable.
#[test]
fn shrinking_remove_applies_when_the_document_exceeds_the_ceiling() {
    let doc = json!({"big": vec![0; 10_000], "s": 1});
    let mut d = doc.clone();
    let mut b = PatchBudget::for_document(
        &d,
        PatchLimits {
            max_depth: 64,
            growth_factor: 1,
            min_nodes: 1,
            max_nodes: 100,
        },
    );
    apply(
        &mut d,
        &ops(json!([{"op": "remove", "path": "/big"}])),
        &mut b,
    )
    .unwrap();
    assert_eq!(d, json!({"s": 1}));
    assert_eq!(b.nodes(), count(&d));
    // The ceiling still binds growth.
    let e = apply(
        &mut d,
        &ops(json!([{"op": "add", "path": "/z", "value": vec![0; 200]}])),
        &mut b,
    )
    .unwrap_err();
    assert!(
        matches!(
            e.reason,
            PatchErrorReason::BudgetExceeded | PatchErrorReason::NodeCeilingExceeded
        ),
        "{e:?}"
    );
    assert_eq!(d, json!({"s": 1}));
}

/// A commit-time failure names the operation that displaced the subtree it
/// could not afford to count, not the last operation.
#[test]
fn commit_budget_failure_names_the_displacing_operation() {
    let doc = json!({"big": vec![0; 1000], "s": vec![0; 1000]});
    let mut d = doc.clone();
    let mut b = pinned(&d);
    let mut p = vec![json!({"op": "remove", "path": "/big"})];
    for _ in 0..7 {
        p.push(json!({"op": "test", "path": "/s", "value": vec![0; 1000]}));
    }
    let e = apply(&mut d, &ops(Value::Array(p)), &mut b).unwrap_err();
    assert_eq!((e.index, e.reason), (0, PatchErrorReason::BudgetExceeded));
    assert!(d == doc);
}

/// A delta that the default limits refuse (walk charged by pointer depth)
/// is replaced by a whole-document replace.
#[test]
fn diff_of_many_deep_replacements_is_applicable() {
    let (before, after) = deep_pair();
    let patch = diff(&before, &after);
    assert_eq!(run(&before, &patch).unwrap(), after);
}

/// Builds `{"d": <100-deep chain ending in 40k scalars>}` and a copy with
/// every scalar changed.
fn deep_pair() -> (Value, Value) {
    fn build(offset: u64) -> Value {
        let leaf: serde_json::Map<String, Value> = (0..40_000u64)
            .map(|i| (format!("k{i}"), json!(i + offset)))
            .collect();
        let mut v = Value::Object(leaf);
        for _ in 0..100 {
            v = json!({ "a": v });
        }
        json!({ "d": v })
    }
    (build(0), build(1))
}

#[test]
fn measured_apply_reports_the_size_of_each_copy_as_it_happens() {
    let mut d = json!({"a": "xy"});
    let mut b = PatchBudget::for_document(&d, PatchLimits::default());
    let p = ops(json!([
        {"op": "copy", "from": "/a", "path": "/b"},
        {"op": "remove", "path": "/a"}
    ]));
    let r = apply_measured(&mut d, &p, &mut b, usize::MAX, true, |_| true);
    // `"xy"` held (one 16-byte block) and the member `b` it was placed in
    // (its key's block; the map's one leaf has room), though its source is
    // gone once the patch applied.
    // The `remove` vacated member `a`: its key's block (the leaf stays).
    assert_eq!(
        r,
        Ok(Measured {
            copied: 32,
            vacated: vec![(1, 16)]
        })
    );
    assert_eq!(d, json!({"b": "xy"}));
    let mut d = json!({"a": "xy"});
    let mut b = PatchBudget::for_document(&d, PatchLimits::default());
    assert_eq!(
        apply_measured(&mut d, &p, &mut b, usize::MAX, false, |_| true),
        Ok(Measured::default())
    );
}

/// Text is charged by length: one unit per 64 bytes of a string or key.
#[test]
fn walk_charges_text_by_length() {
    assert_eq!(walk::count(&json!("x".repeat(6400))), 101);
    assert_eq!(walk::count(&json!({ "k".repeat(640): 0 })), 12);
    assert_eq!(walk::count(&json!("x".repeat(63))), 1);
    let long = json!("x".repeat(6400));
    assert!(matches!(
        walk::clone_bounded(&long, 100, 8),
        Err(walk::Stop::Nodes)
    ));
    assert_eq!(walk::count_displaced(&long, 100), None);
    assert_eq!(walk::count_displaced(&long, 101), Some(101));
    assert!(matches!(
        walk::clone_bounded(&json!({ "k".repeat(6400): 0 }), 100, 8),
        Err(walk::Stop::Nodes)
    ));
}

/// Key and string bytes held by `v` and its descendants.
fn text_bytes(v: &Value) -> usize {
    match v {
        Value::String(s) => s.len(),
        Value::Array(a) => a.iter().map(text_bytes).sum(),
        Value::Object(o) => o.iter().map(|(k, v)| k.len() + text_bytes(v)).sum(),
        _ => 0,
    }
}

/// The documented text bound per node unit: a key and its string value are
/// rounded down separately, so a 63-byte key with a 63-byte string costs one
/// unit (126 bytes), and `n` units hold at most `126n` bytes of text.
#[test]
fn text_per_node_unit_is_at_most_126_bytes() {
    let n = 1000;
    let mut m = serde_json::Map::new();
    for i in 0..n {
        m.insert(format!("{i:063}"), json!(format!("{i:063}")));
    }
    let doc = Value::Object(m);
    assert_eq!(walk::count(&doc), n + 1);
    // The bound itself, on the worst case and on mixed shapes around the
    // 64-byte rounding edges.
    let mut shapes = vec![doc.clone()];
    for len in [0, 1, 63, 64, 65, 127, 128, 129, 1000] {
        let t = "t".repeat(len);
        shapes.push(json!({ t.clone(): t.clone(), "x": [t.clone(), { t.clone(): [t.clone()] }] }));
        shapes.push(json!([t.clone(), t.clone()]));
    }
    for v in &shapes {
        assert!(
            text_bytes(v) <= 126 * walk::count(v),
            "{} text bytes in {} units",
            text_bytes(v),
            walk::count(v)
        );
    }
    // 127-byte key, 127-byte string: 3 units for 64 x 3 + 62 bytes.
    let k = "k".repeat(127);
    assert_eq!(walk::count(&json!({ k: "s".repeat(127) })) - 1, 3);
    // Strings alone stay under 64 bytes per unit.
    assert_eq!(walk::count(&json!("s".repeat(127))), 2);
}

/// A `test` charges the text it compares: a key looked up and an equal-length
/// string compared cost their text units, like a clone of them would.
#[test]
fn test_charges_compared_key_text() {
    let k = "k".repeat(1 << 20);
    let doc = json!({"o": { k.clone(): 0 }});
    let mut d = doc.clone();
    let mut b = PatchBudget::for_document(&d, limits(128, 1, 1));
    // Each `test` compares the whole document's text; the walk budget is
    // 4 x the document, so five of them exceed it.
    let one = json!({"op": "test", "path": "/o", "value": { k: 0 }});
    let p = ops(Value::Array(vec![one; 5]));
    let e = apply(&mut d, &p, &mut b).unwrap_err();
    assert_eq!(e.reason, PatchErrorReason::BudgetExceeded);
    assert_eq!(d, doc);

    let s = "s".repeat(1 << 20);
    let doc = json!({ "s": s.clone() });
    let mut d = doc.clone();
    let mut b = PatchBudget::for_document(&d, limits(128, 1, 1));
    let one = json!({"op": "test", "path": "/s", "value": s});
    let p = ops(Value::Array(vec![one; 5]));
    let e = apply(&mut d, &p, &mut b).unwrap_err();
    assert_eq!(e.reason, PatchErrorReason::BudgetExceeded);
}

/// A `test` looks the operation's own keys up in the document, so a long
/// document key is never hashed for a short expected one: the mismatch is
/// found without charging the document key's text.
#[test]
fn test_does_not_walk_document_keys() {
    let k = "k".repeat(1 << 20);
    let doc = json!({"o": { k.clone(): 0 }, "n": 0});
    let mut d = doc.clone();
    let mut b = PatchBudget::for_document(&d, limits(128, 1, 1));
    // The document is 16,388 units, so the walk budget is 65,552. Three
    // matching tests of /o (16,387 each) and three of /n (2 each) leave
    // 16,385: the last `test` then has 16,383 left after its path and object
    // pair, less than the 16,384 units of the document key it must not walk.
    let mut p = vec![json!({"op": "test", "path": "/o", "value": { k: 0 }}); 3];
    p.extend(vec![json!({"op": "test", "path": "/n", "value": 0}); 3]);
    p.push(json!({"op": "test", "path": "/o", "value": {"z": 0}}));
    let e = apply(&mut d, &ops(Value::Array(p)), &mut b).unwrap_err();
    assert_eq!((e.index, e.reason), (6, PatchErrorReason::TestFailed));
}

/// A `copy` of a long string is charged by its bytes, not as one node.
#[test]
fn copy_of_a_long_string_is_charged_by_bytes() {
    let doc = json!({"s": "x".repeat(64 * 1024)});
    let mut d = doc.clone();
    let mut b = PatchBudget::for_document(&d, limits(128, 1, 3000));
    let p = ops(json!([
        {"op": "copy", "from": "/s", "path": "/c0"},
        {"op": "copy", "from": "/s", "path": "/c1"}
    ]));
    let e = apply(&mut d, &p, &mut b).unwrap_err();
    assert_eq!((e.index, e.reason), (1, PatchErrorReason::BudgetExceeded));
    assert_eq!(d, doc);
}

/// The clone is charged before later operations run: a copy that does not
/// fit is refused as such, not as the failing `test` after it.
#[test]
fn copy_is_charged_by_bytes_before_later_operations_run() {
    let doc = json!({"s": "x".repeat(64 * 1024)});
    let mut d = doc.clone();
    let mut b = PatchBudget::for_document(&d, limits(128, 1, 100));
    let p = ops(json!([
        {"op": "copy", "from": "/s", "path": "/c"},
        {"op": "test", "path": "/s", "value": "y"}
    ]));
    let e = apply(&mut d, &p, &mut b).unwrap_err();
    assert_eq!((e.index, e.reason), (0, PatchErrorReason::BudgetExceeded));
    assert_eq!(d, doc);
}

/// Object keys are text too: the incremental count follows `place` and
/// `take` so it never drifts from a fresh count.
#[test]
fn key_units_track_place_and_take() {
    let k1 = "a".repeat(640);
    let k2 = "b".repeat(640);
    let mut d = json!({});
    let mut b = PatchBudget::for_document(&d, PatchLimits::default());
    // Moves between two keys that both carry units, in both directions:
    // 10 -> 21 units (longer) and 21 -> 11 units (shorter, non-empty).
    let longer = "d".repeat(1344);
    let shorter = "e".repeat(704);
    let steps = [
        json!([{"op": "add", "path": format!("/{k1}"), "value": 1}]),
        json!([{"op": "copy", "from": format!("/{k1}"), "path": format!("/{k2}")}]),
        json!([{"op": "move", "from": format!("/{k1}"), "path": "/short"}]),
        json!([{"op": "move", "from": "/short", "path": format!("/{}", "c".repeat(1280))}]),
        json!([{"op": "move", "from": format!("/{k2}"), "path": format!("/{longer}")}]),
        json!([{"op": "move", "from": format!("/{longer}"), "path": format!("/{shorter}")}]),
        json!([{"op": "remove", "path": format!("/{shorter}")}]),
        json!([{"op": "replace", "path": format!("/{}", "c".repeat(1280)), "value": 2}]),
    ];
    for s in steps {
        apply(&mut d, &ops(s.clone()), &mut b).unwrap();
        assert_eq!(b.nodes(), walk::count(&d), "drift after {s}");
    }
}

/// A `move` to a longer key cannot take the document past the ceiling.
#[test]
fn move_to_a_long_key_respects_the_ceiling() {
    let mut d = json!({"a": 1});
    let mut b = PatchBudget::for_document(
        &d,
        PatchLimits {
            max_depth: 128,
            growth_factor: 1,
            min_nodes: 1,
            max_nodes: 10,
        },
    );
    let p = ops(json!([{"op": "move", "from": "/a", "path": format!("/{}", "k".repeat(1280))}]));
    let e = apply(&mut d, &p, &mut b).unwrap_err();
    assert_eq!(e.reason, PatchErrorReason::NodeCeilingExceeded);
    assert_eq!(d, json!({"a": 1}));
}

/// `diff` charges a new member's key, so it falls back to a root replace
/// when the key text alone would tip the growth over the default cap.
#[test]
fn diff_counts_the_key_of_an_added_member() {
    let before = json!({});
    let after = json!({ "k".repeat(64_000_000): 1 });
    let patch = diff(&before, &after);
    assert!(
        patch.len() == 1 && patch[0].path.as_str().is_empty(),
        "expected a root replace, got {} op(s)",
        patch.len()
    );
}

#[test]
fn diff_of_equal_documents_over_the_ceiling_is_empty() {
    let x = Value::Array(vec![Value::Null; 4_000_001]);
    assert!(diff(&x, &x).is_empty());
}

#[test]
fn diff_removal_over_the_ceiling_stays_a_delta() {
    let big = Value::Array(vec![Value::Null; 4_000_001]);
    let before = json!({"a": 1, "big": big.clone()});
    let after = json!({ "big": big });
    let patch = diff(&before, &after);
    assert!(
        patch.len() == 1 && patch[0].path.as_str() == "/a",
        "expected [remove /a], got {} op(s) at {:?}",
        patch.len(),
        patch.first().map(|o| o.path.as_str().to_owned())
    );
    assert!(run(&before, &patch).unwrap() == after);
}

/// A document already above its ceiling (a text-heavy snapshot, or state
/// whose share the activity content shrank) still takes a delta that does
/// not grow it; only growth past where it started is refused.
#[test]
fn a_document_over_the_ceiling_takes_deltas_that_do_not_grow_it() {
    let doc = json!({"a": (0..10).collect::<Vec<u32>>(), "b": (0..20).collect::<Vec<u32>>()});
    let at_ten = PatchLimits {
        max_depth: 128,
        growth_factor: 1,
        min_nodes: 1,
        max_nodes: 10,
    };
    let mut d = doc.clone();
    let mut b = PatchBudget::for_document(&d, at_ten);
    let start = b.nodes();
    assert!(start > 10);
    let p = ops(json!([{"op": "replace", "path": "/a", "value": 0}]));
    apply(&mut d, &p, &mut b).unwrap();
    assert_eq!(b.nodes(), count(&d));
    assert!(b.nodes() < start);
    let p = ops(json!([{"op": "replace", "path": "/b/0", "value": 7}]));
    apply(&mut d, &p, &mut b).unwrap();
    // Growth past where the patch started is still refused at the ceiling.
    let before = d.clone();
    let p = ops(json!([{"op": "add", "path": "/c", "value": [1, 2]}]));
    let e = apply(&mut d, &p, &mut b).unwrap_err();
    assert_eq!(e.reason, PatchErrorReason::NodeCeilingExceeded);
    assert_eq!(d, before);
    // Net-zero within one patch: replace frees room the add then uses.
    let p = ops(json!([
        {"op": "replace", "path": "/b", "value": 0},
        {"op": "add", "path": "/c", "value": [1, 2]}
    ]));
    apply(&mut d, &p, &mut b).unwrap();
    assert_eq!(b.nodes(), count(&d));
}

/// `diff` charges path text: clearing many members under a long key would
/// repeat that key in every `remove`, so it falls back to a root replace
/// (smaller than such a delta) instead of building it. (Under
/// `preserve-order` the shifts alone already make it replace the object.)
#[test]
fn diff_falls_back_when_repeated_path_text_outgrows_the_document() {
    let key = "k".repeat(16 * 1024);
    let members: serde_json::Map<String, Value> =
        (0..2_000).map(|i| (format!("a{i}"), json!(i))).collect();
    let before = json!({ key.clone(): members });
    let after = json!({ key: {} });
    let patch = diff(&before, &after);
    assert!(
        patch.len() == 1 && (cfg!(feature = "preserve-order") || patch[0].path.as_str().is_empty()),
        "expected a root replace, got {} op(s)",
        patch.len()
    );
    assert!(run(&before, &patch).unwrap() == after);
}

/// `apply` credits what a `replace` displaces before refusing growth, so
/// `diff` counts net growth: scalar changes near the ceiling stay a delta.
#[test]
fn diff_near_the_ceiling_counts_net_growth() {
    let mut before = json!({ "a": Value::Array(vec![Value::Null; 179_983]) });
    for i in 0..10 {
        before[format!("x{i}")] = json!(0);
    }
    assert_eq!(count(&before), 179_995);
    let mut after = before.clone();
    for i in 0..10 {
        after[format!("x{i}")] = json!(1);
    }
    let patch = diff(&before, &after);
    assert_eq!(patch.len(), 10, "expected ten replaces");
    assert!(run(&before, &patch).unwrap() == after);
    // Real growth past the ceiling still falls back.
    after["y"] = Value::Array(vec![Value::Null; 10]);
    let patch = diff(&before, &after);
    assert!(patch.len() == 1 && patch[0].path.as_str().is_empty());
}

/// A 0.9.0 `apply` refuses any growth by a document already above the
/// ceiling, even one the patch's own displaced values pay for, so over the
/// ceiling `diff` keeps falling back whenever its delta adds anything; a
/// pure removal (which every version accepts) stays a delta.
#[test]
fn diff_over_the_ceiling_falls_back_on_any_growth() {
    let mut before = json!({ "a": Value::Array(vec![Value::Null; 180_100]) });
    before["x"] = json!(0);
    assert!(count(&before) > PatchLimits::default().max_nodes);
    let mut after = before.clone();
    after["x"] = json!(1);
    let patch = diff(&before, &after);
    assert!(
        patch.len() == 1 && patch[0].path.as_str().is_empty(),
        "expected a root replace, got {} op(s)",
        patch.len()
    );
    let mut after = before.clone();
    after.as_object_mut().unwrap().remove("x");
    let patch = diff(&before, &after);
    assert_eq!(patch.len(), 1);
    assert_eq!(patch[0].path.as_str(), "/x");
}

/// Under `preserve-order` an object member is a 104-byte `IndexMap` entry
/// whose hash-table index is rewritten as it shifts, so removals are charged
/// one walk unit per 4 members shifted, not per 16 like array elements:
/// 200 head removals on a 100,000-member object (5,000,000 units) exceed the
/// default 4,000,000-unit walk budget.
#[cfg(feature = "preserve-order")]
#[test]
fn object_member_shifts_are_charged_at_their_own_rate() {
    const N: usize = 100_000;
    let doc = wide_object(N);
    let p: Vec<Value> = (0..200)
        .map(|i| json!({"op": "remove", "path": format!("/k{i}")}))
        .collect();
    let Err(e) = run(&doc, &ops(Value::Array(p))) else {
        panic!("200 head removals applied");
    };
    assert_eq!(e.reason, PatchErrorReason::BudgetExceeded);
    assert!(e.index <= 4_000_000 * 4 / N + 4, "late: {}", e.index);
}
