//! `MESSAGES_SNAPSHOT` reconciliation (`events/state.mdx`).

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support_pipeline;
mod support_reducer;

use serde_json::{json, Value};
use support_reducer::Harness;

fn ids(h: &Harness) -> Vec<String> {
    h.history()
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["id"].as_str().unwrap().to_owned())
        .collect()
}

fn snapshot(messages: Value) -> Value {
    json!({"type": "MESSAGES_SNAPSHOT", "messages": messages})
}

fn held() -> Harness {
    Harness::with_history(
        json!([
            {"id": "u1", "role": "user", "content": "hi"},
            {"id": "a1", "role": "assistant", "content": "hello"},
            {"id": "act1", "role": "activity", "activityType": "search", "content": {"q": 1}},
            {"id": "r1", "role": "reasoning", "content": "thinking"},
            {"id": "u2", "role": "user", "content": "bye"}
        ]),
        json!({}),
    )
}

/// covers: STATE-04
#[test]
fn held_message_is_replaced_in_place() {
    let mut h = held();
    let changes = h.push(snapshot(json!([
        {"id": "a1", "role": "assistant", "content": "hello, restated"},
        {"id": "u1", "role": "user", "content": "hi"},
        {"id": "u2", "role": "user", "content": "bye"}
    ])));
    assert!(changes.messages_replaced);
    assert_eq!(ids(&h), ["u1", "a1", "act1", "r1", "u2"]);
    assert_eq!(h.history()[1]["content"], "hello, restated");
}

/// covers: STATE-05
#[test]
fn unseen_messages_append_in_snapshot_order_without_reordering() {
    let mut h = held();
    h.push(snapshot(json!([
        {"id": "n2", "role": "user", "content": "x"},
        {"id": "u2", "role": "user", "content": "bye"},
        {"id": "n1", "role": "assistant", "content": "y"},
        {"id": "u1", "role": "user", "content": "hi"},
        {"id": "a1", "role": "assistant", "content": "hello"}
    ])));
    assert_eq!(ids(&h), ["u1", "a1", "act1", "r1", "u2", "n2", "n1"]);
}

/// covers: STATE-06
#[test]
fn held_messages_absent_from_the_snapshot_are_dropped() {
    let mut h = held();
    h.push(snapshot(
        json!([{"id": "a1", "role": "assistant", "content": "hello"}]),
    ));
    assert_eq!(ids(&h), ["a1", "act1", "r1"]);
    // The index follows: later events find the survivors.
    h.push(json!({"type": "TEXT_MESSAGE_START", "messageId": "a1"}));
    h.push(json!({"type": "TEXT_MESSAGE_CONTENT", "messageId": "a1", "delta": "!"}));
    assert_eq!(h.history()[0]["content"], "hello!");
    assert!(h
        .reducer
        .message(&serde_json::from_value(json!("u1")).unwrap())
        .is_none());
}

/// covers: STATE-07
#[test]
fn client_only_roles_survive_a_snapshot_that_omits_them() {
    let mut h = held();
    h.push(snapshot(json!([])));
    assert_eq!(ids(&h), ["act1", "r1"]);
}

/// covers: STATE-08
#[test]
fn the_exception_is_per_role_and_self_revoking() {
    let mut h = held();
    // Carries an activity message: the held activity set is restated, so act1
    // goes; reasoning is not restated, so r1 stays.
    h.push(snapshot(json!([
        {"id": "act2", "role": "activity", "activityType": "plan", "content": {}}
    ])));
    assert_eq!(ids(&h), ["r1", "act2"]);
    let mut h = held();
    h.push(snapshot(
        json!([{"id": "r2", "role": "reasoning", "content": "new"}]),
    ));
    assert_eq!(ids(&h), ["act1", "r2"]);
}

#[test]
fn tool_call_index_is_rebuilt_from_the_snapshot() {
    let mut h = Harness::new();
    h.push(snapshot(json!([
        {"id": "a1", "role": "assistant", "toolCalls": [
            {"id": "c1", "type": "function", "function": {"name": "f", "arguments": "{"}}
        ]}
    ])));
    h.push(json!({"type": "TOOL_CALL_START", "toolCallId": "c1", "toolCallName": "f", "parentMessageId": "a1"}));
    h.push(json!({"type": "TOOL_CALL_ARGS", "toolCallId": "c1", "delta": "}"}));
    assert_eq!(
        h.history()[0]["toolCalls"][0]["function"]["arguments"],
        "{}"
    );
    assert!(h.codes().is_empty());
}

/// A delta for a message a snapshot dropped is discarded loudly: the stream
/// stays valid, the history does not regain the message, and one warning
/// says so.
#[test]
fn deltas_for_a_message_the_snapshot_dropped_are_reported() {
    use support_pipeline::{finished, run, started};
    let r = run(&[
        started("r1"),
        json!({"type": "TEXT_MESSAGE_START", "messageId": "m", "role": "assistant"}),
        json!({"type": "MESSAGES_SNAPSHOT", "messages": []}),
        json!({"type": "TEXT_MESSAGE_CONTENT", "messageId": "m", "delta": "x"}),
        json!({"type": "TEXT_MESSAGE_END", "messageId": "m"}),
        finished("r1"),
    ]);
    r.assert_ok();
    assert!(r.messages.is_empty(), "{:?}", r.messages);
    let codes: Vec<_> = r.diagnostics.iter().map(|d| d.code.as_str()).collect();
    assert_eq!(codes, ["stream.missing_target"; 2]);
}
