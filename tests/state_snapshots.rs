//! State snapshots and deltas through the reducer (`events/state.mdx`,
//! `basic/patterns/snapshots.mdx`).

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support_pipeline;
mod support_reducer;

use agui_sdk::diagnostics::DiagnosticCode;
use agui_sdk::patch::PatchOp;
use agui_sdk::{ErrorCode, Event};
use serde_json::{json, Value};
use support_reducer::{event, run_finished, run_started, Harness};

fn delta(ops: Value) -> Value {
    json!({"type": "STATE_DELTA", "delta": ops})
}

fn snapshot(v: Value) -> Value {
    json!({"type": "STATE_SNAPSHOT", "snapshot": v})
}

/// covers: SNAP-01, STATE-01
#[test]
fn snapshot_replaces_without_merging() {
    let mut h = Harness::with_history(json!([]), json!({"keep": 1, "shared": {"a": 1}}));
    let changes = h.push(snapshot(json!({"shared": {"b": 2}})));
    assert!(changes.state);
    assert_eq!(h.state(), &json!({"shared": {"b": 2}}));
    assert!(h.codes().is_empty());
}

/// covers: STATE-03
#[test]
fn falsy_and_non_object_snapshots_apply() {
    let mut h = Harness::new();
    for v in [
        json!(0),
        json!(false),
        json!(""),
        json!([]),
        json!(null),
        json!("text"),
    ] {
        h.push(snapshot(v.clone()));
        assert_eq!(h.state(), &v);
    }
    // A delta against a non-object state works too.
    h.push(snapshot(json!([1])));
    h.push(delta(json!([{"op": "add", "path": "/-", "value": 2}])));
    assert_eq!(h.state(), &json!([1, 2]));
}

/// covers: SNAP-05, SNAP-06
#[test]
fn delta_applies_in_order_to_the_current_value() {
    let mut h = Harness::new();
    h.push(snapshot(json!({"items": []})));
    h.push(delta(json!([
        {"op": "add", "path": "/items/0", "value": {"status": "new"}},
        {"op": "replace", "path": "/items/0/status", "value": "done"}
    ])));
    h.push(delta(
        json!([{"op": "copy", "from": "/items/0", "path": "/items/-"}]),
    ));
    assert_eq!(
        h.state(),
        &json!({"items": [{"status": "done"}, {"status": "done"}]})
    );
}

/// covers: STATE-02
#[test]
fn baseline_is_the_input_state_then_the_runs_own_changes() {
    let mut h = Harness::with_history(json!([]), json!({"count": 1}));
    h.push(run_started());
    h.push(delta(
        json!([{"op": "replace", "path": "/count", "value": 2}]),
    ));
    h.push(delta(json!([{"op": "test", "path": "/count", "value": 2}, {"op": "add", "path": "/ok", "value": true}])));
    assert_eq!(h.state(), &json!({"count": 2, "ok": true}));
    // An empty input state means `{}`.
    let mut fresh = Harness::new();
    fresh.push(delta(json!([{"op": "add", "path": "/a", "value": 1}])));
    assert_eq!(fresh.state(), &json!({"a": 1}));
}

/// covers: SNAP-11, SNAP-12, SNAP-13, STATE-11
#[test]
fn unappliable_delta_warns_and_keeps_prior_state() {
    let mut h = Harness::new();
    h.push(snapshot(json!({"a": 1, "list": [1]})));
    let changes = h.push(delta(json!([
        {"op": "replace", "path": "/a", "value": 2},
        {"op": "add", "path": "/list/1", "value": 2},
        {"op": "remove", "path": "/missing"}
    ])));
    assert!(!changes.state);
    assert_eq!(
        h.state(),
        &json!({"a": 1, "list": [1]}),
        "no partial result"
    );
    let d = h.take();
    assert_eq!(d.len(), 1);
    assert_eq!(d[0].code, DiagnosticCode::PatchNotApplied);
    assert_eq!(d[0].pointer, "/delta/2");
    assert_eq!(d[0].event_type, Some(agui_sdk::EventType::StateDelta));
    assert!(
        d[0].message.contains("operation 2 (remove)"),
        "{}",
        d[0].message
    );
    assert!(
        d[0].message.contains("path does not resolve"),
        "{}",
        d[0].message
    );
    // The run carries on: the next delta applies to the prior value.
    h.push(delta(json!([{"op": "replace", "path": "/a", "value": 3}])));
    assert_eq!(h.state(), &json!({"a": 3, "list": [1]}));
}

/// covers: SNAP-14
#[test]
fn failed_test_operation_keeps_prior_state() {
    let mut h = Harness::new();
    h.push(snapshot(json!({"v": 1, "secret": "s3"})));
    h.push(delta(json!([
        {"op": "replace", "path": "/v", "value": 2},
        {"op": "test", "path": "/secret", "value": "guess"}
    ])));
    assert_eq!(h.state(), &json!({"v": 1, "secret": "s3"}));
    let d = h.take();
    assert_eq!(d[0].code, DiagnosticCode::PatchNotApplied);
    assert!(d[0].message.contains("test operation failed"));
    assert!(
        !d[0].message.contains("s3") && !d[0].message.contains("guess"),
        "no payload values"
    );
}

/// covers: SNAP-02, SNAP-16
#[test]
fn next_snapshot_is_adopted_after_a_skipped_delta() {
    let mut h = Harness::new();
    h.push(snapshot(json!({"a": 1})));
    h.push(delta(json!([{"op": "remove", "path": "/nope"}])));
    assert_eq!(h.codes(), ["patch.not_applied"]);
    h.push(snapshot(json!({"b": 2})));
    assert_eq!(h.state(), &json!({"b": 2}));
    // Deltas now apply against the new baseline.
    h.push(delta(json!([{"op": "remove", "path": "/b"}])));
    assert_eq!(h.state(), &json!({}));
    assert!(h.codes().is_empty());
}

/// covers: SNAP-03
#[test]
fn snapshot_may_arrive_at_any_time() {
    let mut h = Harness::new();
    h.push(snapshot(json!({"early": true})));
    h.push(run_started());
    h.push(json!({"type": "TEXT_MESSAGE_START", "messageId": "m1"}));
    h.push(snapshot(json!({"mid": true})));
    h.push(json!({"type": "TEXT_MESSAGE_CONTENT", "messageId": "m1", "delta": "hi"}));
    h.push(json!({"type": "TEXT_MESSAGE_END", "messageId": "m1"}));
    h.push(snapshot(json!({"late": true})));
    assert_eq!(h.state(), &json!({"late": true}));
    assert_eq!(h.history()[0]["content"], "hi");
}

/// covers: SNAP-08
#[test]
fn unknown_operation_members_are_kept_and_ignored() {
    let raw = json!({"type": "STATE_DELTA", "delta": [
        {"op": "add", "path": "/a", "value": 1, "x-origin": {"tool": "planner"}}
    ]});
    let e = event(raw.clone());
    let Event::StateDelta(d) = &e else {
        panic!("not a delta")
    };
    assert_eq!(
        d.delta[0].extensions.get("x-origin"),
        Some(&json!({"tool": "planner"}))
    );
    assert_eq!(
        serde_json::to_value(&e).unwrap(),
        raw,
        "extras re-serialize"
    );
    let mut h = Harness::new();
    h.push(raw);
    assert_eq!(h.state(), &json!({"a": 1}));
}

/// covers: SNAP-08
#[test]
fn an_undefined_from_member_is_kept_whole() {
    // RFC 6902 defines `from` only for `move` and `copy`; on any other op it
    // is an undefined member, of any type and `null` included, and arrives
    // whole on both push paths.
    for op in [
        json!({"op": "remove", "path": "/a", "from": 5}),
        json!({"op": "remove", "path": "/a", "from": null}),
        json!({"op": "remove", "path": "/a", "from": "/x"}),
        json!({"op": "add", "path": "/b", "value": 1, "from": true}),
        json!({"op": "replace", "path": "/a", "value": 2, "from": [1]}),
        json!({"op": "test", "path": "/a", "value": 0, "from": {}}),
    ] {
        let raw = delta(json!([op.clone()]));
        let e =
            serde_json::from_value::<Event>(raw.clone()).unwrap_or_else(|e| panic!("{op}: {e}"));
        assert_eq!(serde_json::to_value(&e).unwrap(), raw, "{op}");
        let stream = support_pipeline::in_run(&[snapshot(json!({"a": 0})), raw.clone()]);
        for r in [
            support_pipeline::run(&stream),
            support_pipeline::run_value(&stream),
        ] {
            r.assert_ok();
            let delivered = r
                .wire()
                .into_iter()
                .find(|w| w["type"] == "STATE_DELTA")
                .unwrap_or_else(|| panic!("{op}: no delta delivered"));
            assert_eq!(delivered["delta"][0], op, "{op}");
        }
    }
    let activity = json!({"type": "ACTIVITY_DELTA", "messageId": "a", "activityType": "s",
        "patch": [{"op": "remove", "path": "/k", "from": 5}]});
    let e = serde_json::from_value::<Event>(activity.clone()).unwrap();
    assert_eq!(serde_json::to_value(&e).unwrap(), activity);
    let stream = support_pipeline::in_run(&[
        json!({"type": "ACTIVITY_SNAPSHOT", "messageId": "a", "activityType": "s", "content": {"k": 1}}),
        activity.clone(),
    ]);
    for r in [
        support_pipeline::run(&stream),
        support_pipeline::run_value(&stream),
    ] {
        r.assert_ok();
        assert_eq!(r.message("a")["content"], json!({}));
    }
}

/// covers: SNAP-09
#[test]
fn malformed_operations_cannot_become_a_typed_delta() {
    for bad in [
        json!({"type": "STATE_DELTA", "delta": {"op": "add", "path": "/a", "value": 1}}),
        json!({"type": "STATE_DELTA", "delta": [1]}),
        json!({"type": "STATE_DELTA", "delta": [{"path": "/a", "value": 1}]}),
        json!({"type": "STATE_DELTA", "delta": [{"op": "add", "value": 1}]}),
        json!({"type": "STATE_DELTA", "delta": [{"op": "add", "path": 7, "value": 1}]}),
        json!({"type": "STATE_DELTA", "delta": [{"op": 3, "path": "/a", "value": 1}]}),
    ] {
        assert!(
            serde_json::from_value::<Event>(bad.clone()).is_err(),
            "{bad}"
        );
        // Through the consumer pipeline, on both entry paths: fatal, never
        // stripped, and the state is untouched.
        let stream = support_pipeline::in_run(&[snapshot(json!({"a": 0})), bad.clone()]);
        for r in [
            support_pipeline::run(&stream),
            support_pipeline::run_value(&stream),
        ] {
            assert!(
                matches!(
                    r.error.as_ref().map(|e| e.code()),
                    Some(ErrorCode::InvalidValue | ErrorCode::MalformedEvent)
                ),
                "{bad}: {:?}",
                r.types()
            );
            assert_eq!(r.state, json!({"a": 0}), "{bad}");
            assert!(!r.types().contains(&"STATE_DELTA"), "{bad}");
        }
    }
}

/// covers: SNAP-10
#[test]
fn unknown_op_never_reaches_the_reducer_and_the_rest_applies() {
    // The typed operation has no room for an unknown `op`: enforcement drops
    // that element with a warning before typing, and the rest applies.
    assert!(serde_json::from_value::<PatchOp>(json!("x-merge")).is_err());
    let stream = support_pipeline::in_run(&[delta(json!([
        {"op": "x-merge", "path": "/dropped", "value": {"y": 1}},
        {"op": "add", "path": "/kept", "value": true}
    ]))]);
    for r in [
        support_pipeline::run(&stream),
        support_pipeline::run_value(&stream),
    ] {
        r.assert_ok();
        assert_eq!(r.state, json!({"kept": true}));
        assert_eq!(
            r.wire()[1]["delta"],
            json!([{"op": "add", "path": "/kept", "value": true}])
        );
        assert!(r
            .diagnostics
            .iter()
            .any(|d| d.code == DiagnosticCode::UnknownPatchOp && d.pointer == "/delta/0"));
    }
}

/// covers: STATE-10
#[test]
fn state_and_history_persist_across_runs() {
    let mut h = Harness::new();
    h.push(run_started());
    h.push(snapshot(json!({"n": 1})));
    h.push(json!({"type": "TEXT_MESSAGE_START", "messageId": "m1"}));
    h.push(json!({"type": "TEXT_MESSAGE_END", "messageId": "m1"}));
    h.push(run_finished());
    h.push(json!({"type": "RUN_STARTED", "threadId": "t1", "runId": "r2"}));
    h.push(delta(json!([{"op": "replace", "path": "/n", "value": 2}])));
    h.push(json!({"type": "TEXT_MESSAGE_START", "messageId": "m2"}));
    h.push(json!({"type": "TEXT_MESSAGE_END", "messageId": "m2"}));
    h.push(json!({"type": "RUN_FINISHED", "threadId": "t1", "runId": "r2"}));
    assert_eq!(h.state(), &json!({"n": 2}));
    assert_eq!(h.reducer.messages().len(), 2);
    let ids: Vec<&str> = h
        .reducer
        .run_message_ids()
        .iter()
        .map(|id| id.as_str())
        .collect();
    assert_eq!(ids, ["m2"]);
    let (messages, state) = h.reducer.into_parts();
    assert_eq!((messages.len(), state), (2, json!({"n": 2})));
}

/// covers: STATE-12
#[test]
fn state_is_stored_as_inert_data() {
    // The reducer never interprets state: markup and code are stored as text
    // for the application to validate before acting on them.
    let mut h = Harness::new();
    let hostile =
        json!({"html": "<script>alert(1)</script>", "cmd": "rm -rf /", "grant": {"admin": true}});
    h.push(snapshot(hostile.clone()));
    assert_eq!(h.state(), &hostile);
    assert!(h.reducer.messages().is_empty());
    assert!(h.reducer.pending_tool_calls().is_empty());
}

#[test]
fn budget_refusal_is_a_warning_not_a_failure() {
    let mut h = Harness::new();
    h.push(snapshot(json!({"a": 1})));
    let deep = (0..200).fold(json!(0), |v, _| json!({ "n": v }));
    h.push(delta(
        json!([{"op": "add", "path": "/deep", "value": deep}]),
    ));
    assert_eq!(h.codes(), ["patch.budget_exceeded"]);
    assert_eq!(h.state(), &json!({"a": 1}));
}
