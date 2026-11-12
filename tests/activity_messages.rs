//! Activity snapshots and deltas (`events/activity.mdx`).

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[cfg(feature = "client")]
mod support_client;
mod support_pipeline;
mod support_reducer;

use agui_sdk::diagnostics::DiagnosticCode;
use agui_sdk::{Event, RunAgentInput};
use serde_json::{json, Value};
use support_reducer::Harness;

fn snap(id: &str, kind: &str, content: Value) -> Value {
    json!({"type": "ACTIVITY_SNAPSHOT", "messageId": id, "activityType": kind, "content": content})
}

fn delta(id: &str, kind: &str, patch: Value) -> Value {
    json!({"type": "ACTIVITY_DELTA", "messageId": id, "activityType": kind, "patch": patch})
}

/// covers: ACT-01, ACT-17
#[test]
fn unknown_activity_type_is_tolerated_with_role_activity() {
    let mut h = Harness::new();
    h.push(snap("a1", "x-vendor-widget-never-seen", json!({"k": 1})));
    assert_eq!(
        h.history(),
        json!([{"id": "a1", "role": "activity", "activityType": "x-vendor-widget-never-seen", "content": {"k": 1}}])
    );
    assert!(h.codes().is_empty());
}

/// covers: ACT-02
#[test]
fn snapshot_for_an_unseen_id_creates_at_arrival_position() {
    let mut h = Harness::new();
    h.push(json!({"type": "TEXT_MESSAGE_START", "messageId": "m1"}));
    h.push(json!({"type": "TEXT_MESSAGE_END", "messageId": "m1"}));
    h.push(snap("act", "search", json!({"found": 0})));
    h.push(json!({"type": "TEXT_MESSAGE_START", "messageId": "m2"}));
    h.push(delta(
        "act",
        "search",
        json!([{"op": "replace", "path": "/found", "value": 3}]),
    ));
    let history = h.history();
    assert_eq!(history[1]["id"], "act");
    assert_eq!(history[1]["content"], json!({"found": 3}));
    assert_eq!(history[2]["id"], "m2");
}

/// covers: ACT-03
#[test]
fn snapshot_replaces_content_and_type_by_default() {
    let mut h = Harness::new();
    h.push(snap("a", "search", json!({"q": "x", "n": 1})));
    h.push(snap("a", "results", json!({"hits": []})));
    // No `replace` at all: replaced.
    assert_eq!(h.history()[0]["activityType"], "results");
    assert_eq!(h.history()[0]["content"], json!({"hits": []}));
    let mut explicit = snap("a", "done", json!({"ok": true}));
    explicit["replace"] = json!(true);
    h.push(explicit);
    assert_eq!(h.history()[0]["activityType"], "done");
    assert_eq!(h.history()[0]["content"], json!({"ok": true}));
}

/// covers: ACT-04
#[test]
fn replace_false_preserves_the_existing_message() {
    let mut h = Harness::new();
    let mut first = snap("a", "search", json!({"q": "x"}));
    first["replace"] = json!(false);
    h.push(first);
    assert_eq!(
        h.history()[0]["content"],
        json!({"q": "x"}),
        "creating applies its values"
    );
    let mut second = snap("a", "other", json!({"z": 1}));
    second["replace"] = json!(false);
    h.push(second);
    assert_eq!(h.history()[0]["activityType"], "search");
    assert_eq!(h.history()[0]["content"], json!({"q": "x"}));
}

/// covers: ACT-05
#[test]
fn replacement_keeps_accumulated_metadata() {
    let mut h = Harness::new();
    let mut a = snap("a", "s", json!({}));
    a["metadata"] = json!({"first": 1});
    h.push(a);
    let mut b = snap("a", "s", json!({"n": 2}));
    b["metadata"] = json!({"second": 2});
    h.push(b);
    assert_eq!(h.history()[0]["metadata"], json!({"first": 1, "second": 2}));
}

/// covers: ACT-06
#[test]
fn replacing_snapshot_re_mints_ownership() {
    let mut h = Harness::new();
    let mut a = snap("a", "s", json!({}));
    a["subagentRunId"] = json!("sub-1");
    h.push(a);
    let mut b = snap("a", "s", json!({}));
    b["subagentRunId"] = json!("sub-2");
    h.push(b);
    assert_eq!(h.history()[0]["subagentRunId"], "sub-2");
    h.push(snap("a", "s", json!({})));
    assert!(
        h.history()[0].get("subagentRunId").is_none(),
        "absent re-mints as parent"
    );
}

/// covers: ACT-07
#[test]
fn non_replacing_snapshot_keeps_the_owner() {
    let mut h = Harness::new();
    let mut a = snap("a", "s", json!({}));
    a["subagentRunId"] = json!("sub-1");
    h.push(a);
    let mut b = snap("a", "s", json!({}));
    b["replace"] = json!(false);
    h.push(b);
    assert_eq!(h.history()[0]["subagentRunId"], "sub-1");
}

/// covers: ACT-07
#[test]
fn non_replacing_snapshot_does_not_hand_the_activity_to_another_owner() {
    use agui_sdk::ErrorCode;
    use support_pipeline::{in_run, run};
    let mut takeover = snap("a1", "PLAN", json!({}));
    takeover["replace"] = json!(false);
    takeover["subagentRunId"] = json!("s1");
    let mut write = delta("a1", "PLAN", json!([]));
    write["subagentRunId"] = json!("s1");
    let r = run(&in_run(&[snap("a1", "PLAN", json!({})), takeover, write]));
    r.assert_rejected("does not match");
    assert_eq!(
        r.error.as_ref().map(|e| e.code()),
        Some(ErrorCode::Ownership)
    );
}

/// covers: ACT-09
#[test]
fn delta_may_retype_the_activity() {
    let mut h = Harness::new();
    h.push(snap("a", "search", json!({"n": 0})));
    h.push(delta(
        "a",
        "results",
        json!([{"op": "add", "path": "/hits", "value": [1]}]),
    ));
    assert_eq!(h.history()[0]["activityType"], "results");
    assert_eq!(h.history()[0]["content"], json!({"n": 0, "hits": [1]}));
}

/// covers: ACT-12, ACT-13, ACT-14
#[test]
fn delta_for_missing_or_non_activity_message_is_skipped_with_a_warning() {
    let mut h = Harness::new();
    h.push(json!({"type": "TEXT_MESSAGE_START", "messageId": "m1"}));
    let before = h.history();
    let changes = h.push(delta(
        "nope",
        "s",
        json!([{"op": "add", "path": "/a", "value": 1}]),
    ));
    assert_eq!(changes.message, None);
    h.push(delta(
        "m1",
        "s",
        json!([{"op": "add", "path": "/a", "value": 1}]),
    ));
    assert_eq!(h.history(), before, "no message invented or changed");
    let d = h.take();
    assert_eq!(d.len(), 2);
    assert!(d
        .iter()
        .all(|d| d.code == DiagnosticCode::MissingActivityTarget));
    assert_eq!(d[0].pointer, "/messageId");
    // The reducer keeps going.
    h.push(snap("a", "s", json!({})));
    assert_eq!(h.reducer.messages().len(), 2);
}

/// covers: ACT-16
#[test]
fn failed_patch_keeps_content_and_a_fresh_snapshot_resyncs() {
    let mut h = Harness::new();
    h.push(snap("a", "s", json!({"n": 1})));
    h.push(delta(
        "a",
        "retyped",
        json!([
            {"op": "replace", "path": "/n", "value": 2},
            {"op": "test", "path": "/n", "value": 99}
        ]),
    ));
    assert_eq!(h.history()[0]["content"], json!({"n": 1}));
    assert_eq!(
        h.history()[0]["activityType"],
        "s",
        "a skipped delta does not retype"
    );
    let d = h.take();
    assert_eq!(d[0].code, DiagnosticCode::PatchNotApplied);
    assert_eq!(d[0].pointer, "/patch/1");
    h.push(snap("a", "s", json!({"n": 7})));
    h.push(delta(
        "a",
        "s",
        json!([{"op": "replace", "path": "/n", "value": 8}]),
    ));
    assert_eq!(h.history()[0]["content"], json!({"n": 8}));
}

#[test]
fn patch_that_would_leave_a_non_object_is_refused() {
    let mut h = Harness::new();
    h.push(snap("a", "s", json!({"n": 1})));
    h.push(delta(
        "a",
        "s",
        json!([{"op": "replace", "path": "", "value": [1]}]),
    ));
    assert_eq!(h.history()[0]["content"], json!({"n": 1}));
    assert_eq!(h.codes(), ["patch.not_applied"]);
    // A root replacement by another object is fine.
    h.push(delta(
        "a",
        "s",
        json!([{"op": "replace", "path": "", "value": {"m": 2}}]),
    ));
    assert_eq!(h.history()[0]["content"], json!({"m": 2}));
}

/// covers: ACT-18
#[test]
fn activity_messages_are_stripped_from_run_input() {
    let mut h = Harness::new();
    h.push(json!({"type": "TEXT_MESSAGE_START", "messageId": "m1", "role": "user"}));
    h.push(snap("a", "s", json!({})));
    let input: RunAgentInput = serde_json::from_value(json!({
        "threadId": "t", "runId": "r", "state": {}, "messages": h.history(),
        "tools": [], "context": [], "forwardedProps": {}
    }))
    .unwrap();
    let sendable: Vec<_> = input.sendable_messages().collect();
    assert_eq!(sendable.len(), 1);
    assert_eq!(serde_json::to_value(sendable[0]).unwrap()["id"], "m1");
}

/// covers: ACT-18
#[cfg(feature = "client")]
#[test]
fn the_client_never_sends_activity_messages() {
    use std::future::IntoFuture;

    use agui_sdk::client::middleware::{MiddlewareRequest, Next};
    use agui_sdk::client::{AgentClient, RunRequest};
    use agui_sdk::message::{ActivityMessage, Message};
    use support_client::{block_on, mock, run_of};

    // A history holding an activity message, as a previous run's report
    // would hand it back, sent on both client paths.
    let history = vec![
        Message::user("hi"),
        Message::Activity(ActivityMessage::new(
            "a1",
            "progress",
            serde_json::from_value(json!({"pct": 50})).unwrap(),
        )),
        Message::user("again"),
    ];
    for with_middleware in [false, true] {
        let transport = mock(run_of("t1", "r1", vec![]));
        let mut builder = AgentClient::with_transport(transport.clone())
            .diagnostic_sink(agui_sdk::diagnostics::NullSink);
        if with_middleware {
            builder = builder.middleware(|r: MiddlewareRequest, n: Next| n.run(r));
        }
        let client = builder.build().unwrap();
        let request = RunRequest::new()
            .thread_id("t1")
            .run_id("r1")
            .messages(history.clone());
        let report = block_on(client.run(request).into_future()).unwrap();
        let sent = transport.last_request().unwrap();
        let roles: Vec<&Value> = sent["messages"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| &m["role"])
            .collect();
        assert_eq!(roles, [&json!("user"), &json!("user")], "{sent}");
        // The client's own history keeps it: it is stripped from the wire,
        // not forgotten.
        assert!(report.messages.iter().any(|m| m.id().as_str() == "a1"));
    }
}

/// covers: ACT-19
#[test]
fn malformed_activity_events_cannot_be_typed() {
    for bad in [
        json!({"type": "ACTIVITY_SNAPSHOT", "messageId": "a", "activityType": "s", "content": [1]}),
        json!({"type": "ACTIVITY_SNAPSHOT", "messageId": "a", "content": {}}),
        json!({"type": "ACTIVITY_DELTA", "messageId": "a", "activityType": "s", "patch": {}}),
    ] {
        assert!(
            serde_json::from_value::<Event>(bad.clone()).is_err(),
            "{bad}"
        );
        // Through the consumer pipeline, on both entry paths: fatal, and no
        // activity message appears.
        let stream = support_pipeline::in_run(std::slice::from_ref(&bad));
        for r in [
            support_pipeline::run(&stream),
            support_pipeline::run_value(&stream),
        ] {
            assert!(
                matches!(
                    r.error.as_ref().map(|e| e.code()),
                    Some(agui_sdk::ErrorCode::InvalidValue | agui_sdk::ErrorCode::MalformedEvent)
                ),
                "{bad}: {:?}",
                r.types()
            );
            assert!(r.messages.is_empty(), "{bad}");
        }
    }
}
