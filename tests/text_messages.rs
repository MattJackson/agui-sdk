//! Text messages, consumer side (`events/text-messages.mdx`).

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support_pipeline;

use agui_sdk::ErrorCode;
use serde_json::json;
use support_pipeline::{in_run, run, text};

/// covers: TEXT-02, TEXT-04
#[test]
fn a_message_is_matched_by_id_and_its_deltas_concatenate() {
    let r = run(&in_run(&[
        json!({"type": "TEXT_MESSAGE_START", "messageId": "a"}),
        json!({"type": "TEXT_MESSAGE_START", "messageId": "b"}),
        json!({"type": "TEXT_MESSAGE_CONTENT", "messageId": "a", "delta": "1"}),
        json!({"type": "TEXT_MESSAGE_CONTENT", "messageId": "b", "delta": "x"}),
        json!({"type": "TEXT_MESSAGE_CONTENT", "messageId": "a", "delta": "2"}),
        json!({"type": "TEXT_MESSAGE_END", "messageId": "b"}),
        json!({"type": "TEXT_MESSAGE_CONTENT", "messageId": "a", "delta": "3"}),
        json!({"type": "TEXT_MESSAGE_END", "messageId": "a"}),
    ]));
    r.assert_ok();
    assert_eq!(r.message("a")["content"], "123");
    assert_eq!(r.message("b")["content"], "x");
}

/// covers: TEXT-03
#[test]
fn start_role_defaults_to_assistant_and_name_is_kept() {
    let r = run(&in_run(&[
        json!({"type": "TEXT_MESSAGE_START", "messageId": "m1", "name": "planner"}),
        json!({"type": "TEXT_MESSAGE_CONTENT", "messageId": "m1", "delta": "hi"}),
        json!({"type": "TEXT_MESSAGE_END", "messageId": "m1"}),
    ]));
    r.assert_ok();
    assert_eq!(r.message("m1")["role"], "assistant");
    assert_eq!(r.message("m1")["name"], "planner");
}

/// covers: TEXT-05
#[test]
fn a_closed_message_reopens_and_further_content_appends() {
    let mut body = text("m1", "First half.").to_vec();
    body.extend(text("m1", " Second half."));
    let r = run(&in_run(&body));
    r.assert_ok();
    assert_eq!(r.messages.len(), 1);
    assert_eq!(r.message("m1")["content"], "First half. Second half.");
}

/// covers: TEXT-07
#[test]
fn a_disagreeing_reopen_is_not_fatal_and_established_values_stand() {
    let r = run(&in_run(&[
        json!({"type": "TEXT_MESSAGE_START", "messageId": "m1", "role": "assistant", "name": "a"}),
        json!({"type": "TEXT_MESSAGE_CONTENT", "messageId": "m1", "delta": "one"}),
        json!({"type": "TEXT_MESSAGE_END", "messageId": "m1"}),
        json!({"type": "TEXT_MESSAGE_START", "messageId": "m1", "role": "user", "name": "b"}),
        json!({"type": "TEXT_MESSAGE_CONTENT", "messageId": "m1", "delta": " two"}),
        json!({"type": "TEXT_MESSAGE_END", "messageId": "m1"}),
    ]));
    r.assert_ok();
    let m1 = r.message("m1");
    assert_eq!(m1["role"], "assistant");
    assert_eq!(m1["name"], "a");
    assert_eq!(m1["content"], "one two");
}

/// covers: TEXT-08
#[test]
fn a_messages_snapshot_restates_a_closed_message_wholesale() {
    let mut body = text("m1", "draft").to_vec();
    body.push(json!({"type": "MESSAGES_SNAPSHOT", "messages": [
        {"id": "m1", "role": "assistant", "content": "final"}
    ]}));
    let r = run(&in_run(&body));
    r.assert_ok();
    assert_eq!(r.message("m1")["content"], "final");
}

/// covers: TEXT-09
#[test]
fn text_chunks_expand_with_first_chunk_and_conflict_rules() {
    let r = run(&in_run(&[
        json!({"type": "TEXT_MESSAGE_CHUNK", "messageId": "m1", "role": "assistant", "delta": "Chunks "}),
        json!({"type": "TEXT_MESSAGE_CHUNK", "delta": "expand."}),
    ]));
    r.assert_ok();
    assert_eq!(r.message("m1")["content"], "Chunks expand.");

    run(&in_run(&[
        json!({"type": "TEXT_MESSAGE_CHUNK", "delta": "x"}),
    ]))
    .assert_rejected("First TEXT_MESSAGE_CHUNK must have a messageId");
    run(&in_run(&[
        json!({"type": "TEXT_MESSAGE_CHUNK", "messageId": "m1", "role": "user", "delta": "a"}),
        json!({"type": "TEXT_MESSAGE_CHUNK", "messageId": "m1", "role": "assistant", "delta": "b"}),
    ]))
    .assert_rejected("does not match the open stream's role");
}

/// covers: TEXT-10
#[test]
fn metadata_on_any_event_merges_into_the_message() {
    let r = run(&in_run(&[
        json!({"type": "TEXT_MESSAGE_START", "messageId": "m1", "metadata": {"a": 1, "b": 1}}),
        json!({"type": "TEXT_MESSAGE_CONTENT", "messageId": "m1", "delta": "x", "metadata": {"b": 2}}),
        json!({"type": "TEXT_MESSAGE_END", "messageId": "m1", "metadata": {"c": 3}}),
    ]));
    r.assert_ok();
    assert_eq!(r.message("m1")["metadata"], json!({"a": 1, "b": 2, "c": 3}));
}

/// covers: TEXT-11
#[test]
fn sequence_violations_are_fatal() {
    let r = run(&in_run(&[
        json!({"type": "TEXT_MESSAGE_CONTENT", "messageId": "orphan-content-m1", "delta": "nothing is open"}),
    ]));
    r.assert_rejected("No active text message found with ID 'orphan-content-m1'");
    assert_eq!(
        r.error.as_ref().map(|e| e.code()),
        Some(ErrorCode::TextMessage)
    );
    run(&in_run(&[
        json!({"type": "TEXT_MESSAGE_END", "messageId": "m9"}),
    ]))
    .assert_rejected("No active text message found with ID 'm9'");
    run(&in_run(&[
        json!({"type": "TEXT_MESSAGE_START", "messageId": "m1"}),
        json!({"type": "TEXT_MESSAGE_START", "messageId": "m1"}),
    ]))
    .assert_rejected("A text message with ID 'm1' is already in progress");
    run(&in_run(&[
        json!({"type": "TEXT_MESSAGE_START", "messageId": "m1"}),
    ]))
    .assert_rejected("Cannot send 'RUN_FINISHED' while text messages are still active: 'm1'");
}

/// covers: TEXT-12
#[test]
fn the_assembled_message_takes_its_role() {
    for role in ["assistant", "user", "system", "developer"] {
        let r = run(&in_run(&[
            json!({"type": "TEXT_MESSAGE_START", "messageId": "m1", "role": role}),
            json!({"type": "TEXT_MESSAGE_CONTENT", "messageId": "m1", "delta": "x"}),
            json!({"type": "TEXT_MESSAGE_END", "messageId": "m1"}),
        ]));
        r.assert_ok();
        assert_eq!(r.message("m1")["role"], role);
        assert_eq!(r.message("m1")["content"], "x");
    }
}
