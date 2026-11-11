//! Reasoning spans and messages, consumer side (`events/reasoning.mdx`).

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support_pipeline;

use agui_sdk::ErrorCode;
use serde_json::{json, Value};
use support_pipeline::{in_run, run};

fn rmsg(id: &str, delta: &str) -> [Value; 3] {
    [
        json!({"type": "REASONING_MESSAGE_START", "messageId": id, "role": "reasoning"}),
        json!({"type": "REASONING_MESSAGE_CONTENT", "messageId": id, "delta": delta}),
        json!({"type": "REASONING_MESSAGE_END", "messageId": id}),
    ]
}

/// covers: REAS-05
#[test]
fn a_span_holds_several_messages_with_their_own_ids() {
    let mut body = vec![json!({"type": "REASONING_START", "messageId": "span-1"})];
    body.extend(rmsg("r1", "first"));
    body.extend(rmsg("r2", "second"));
    body.push(json!({"type": "REASONING_END", "messageId": "span-1"}));
    let r = run(&in_run(&body));
    r.assert_ok();
    assert_eq!(r.message("r1")["content"], "first");
    assert_eq!(r.message("r2")["content"], "second");
    assert!(
        r.messages.iter().all(|m| m["id"] != "span-1"),
        "a span is not a message"
    );
}

/// covers: REAS-06
#[test]
fn a_reasoning_message_role_other_than_reasoning_is_malformed() {
    let r = run(&in_run(&[
        json!({"type": "REASONING_MESSAGE_START", "messageId": "r1", "role": "assistant"}),
        json!({"type": "REASONING_MESSAGE_END", "messageId": "r1"}),
    ]));
    assert_eq!(
        r.error.as_ref().map(|e| e.code()),
        Some(ErrorCode::InvalidValue)
    );
    assert!(r.messages.is_empty());
}

/// covers: REAS-08, REAS-09
#[test]
fn an_orphaned_fragment_fails_the_run_and_invents_nothing() {
    let r = run(&in_run(&[
        json!({"type": "REASONING_MESSAGE_CONTENT", "messageId": "m-reasoning-discipline-1", "delta": "orphaned thought"}),
    ]));
    r.assert_rejected("No active reasoning message found with ID 'm-reasoning-discipline-1'");
    assert_eq!(
        r.error.as_ref().map(|e| e.code()),
        Some(ErrorCode::Reasoning)
    );
    assert!(r.messages.is_empty());
    run(&in_run(&[
        json!({"type": "REASONING_MESSAGE_END", "messageId": "r1"}),
    ]))
    .assert_rejected("No active reasoning message found with ID 'r1'");
}

/// covers: REAS-10
#[test]
fn span_discipline_is_enforced() {
    run(&in_run(&[
        json!({"type": "REASONING_END", "messageId": "s1"}),
    ]))
    .assert_rejected("No active reasoning span found with ID 's1'");
    run(&in_run(&[
        json!({"type": "REASONING_START", "messageId": "s1"}),
        json!({"type": "REASONING_START", "messageId": "s1"}),
    ]))
    .assert_rejected("A reasoning span with ID 's1' is already in progress");
}

/// covers: REAS-11
#[test]
fn reopening_an_open_reasoning_message_is_rejected() {
    run(&in_run(&[
        json!({"type": "REASONING_MESSAGE_START", "messageId": "r1", "role": "reasoning"}),
        json!({"type": "REASONING_MESSAGE_START", "messageId": "r1", "role": "reasoning"}),
    ]))
    .assert_rejected("A reasoning message with ID 'r1' is already in progress");
    // Closed, it may be used again.
    let mut body = rmsg("r1", "a").to_vec();
    body.extend(rmsg("r1", "b"));
    run(&in_run(&body)).assert_ok();
}

/// covers: REAS-12
#[test]
fn spans_and_messages_are_separate_namespaces() {
    let mut body = vec![json!({"type": "REASONING_START", "messageId": "same"})];
    body.extend(rmsg("same", "shared id"));
    body.push(json!({"type": "REASONING_END", "messageId": "same"}));
    run(&in_run(&body)).assert_ok();

    // A fragment naming only a span is orphaned.
    run(&in_run(&[
        json!({"type": "REASONING_START", "messageId": "s1"}),
        json!({"type": "REASONING_MESSAGE_CONTENT", "messageId": "s1", "delta": "x"}),
    ]))
    .assert_rejected("No active reasoning message found with ID 's1'");
    // And closing a message does not close the span of the same id.
    run(&in_run(&[
        json!({"type": "REASONING_MESSAGE_START", "messageId": "x", "role": "reasoning"}),
        json!({"type": "REASONING_END", "messageId": "x"}),
    ]))
    .assert_rejected("No active reasoning span found with ID 'x'");
}

/// covers: REAS-13
#[test]
fn open_reasoning_at_run_finish_fails_the_run() {
    run(&in_run(&[
        json!({"type": "REASONING_START", "messageId": "s1"}),
    ]))
    .assert_rejected("Cannot send 'RUN_FINISHED' while reasoning spans are still active: 's1'");
    run(&in_run(&[
        json!({"type": "REASONING_MESSAGE_START", "messageId": "r1", "role": "reasoning"}),
    ]))
    .assert_rejected("Cannot send 'RUN_FINISHED' while reasoning messages are still active: 'r1'");
}
