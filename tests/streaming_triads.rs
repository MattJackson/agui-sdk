//! Open/content/close triads (`basic/patterns/streaming.mdx`).

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support_pipeline;

use serde_json::json;
use support_pipeline::{in_run, run};

/// covers: STRM-04
#[test]
fn deltas_concatenate_in_arrival_order() {
    let r = run(&in_run(&[
        json!({"type": "TEXT_MESSAGE_START", "messageId": "m1", "role": "assistant"}),
        json!({"type": "TEXT_MESSAGE_CONTENT", "messageId": "m1", "delta": "Hello, "}),
        json!({"type": "TEXT_MESSAGE_CONTENT", "messageId": "m1", "delta": "world."}),
        json!({"type": "TEXT_MESSAGE_END", "messageId": "m1"}),
        json!({"type": "TOOL_CALL_START", "toolCallId": "t1", "toolCallName": "search", "parentMessageId": "m1"}),
        json!({"type": "TOOL_CALL_ARGS", "toolCallId": "t1", "delta": "{\"q\":"}),
        json!({"type": "TOOL_CALL_ARGS", "toolCallId": "t1", "delta": "\"agui\"}"}),
        json!({"type": "TOOL_CALL_END", "toolCallId": "t1"}),
    ]));
    r.assert_ok();
    let m1 = r.message("m1");
    assert_eq!(m1["content"], "Hello, world.");
    assert_eq!(m1["toolCalls"][0]["function"]["name"], "search");
    assert_eq!(
        m1["toolCalls"][0]["function"]["arguments"],
        "{\"q\":\"agui\"}"
    );
}

/// covers: STRM-05
#[test]
fn messages_calls_reasoning_and_steps_interleave() {
    let r = run(&in_run(&[
        json!({"type": "STEP_STARTED", "stepName": "think"}),
        json!({"type": "TEXT_MESSAGE_START", "messageId": "m1"}),
        json!({"type": "REASONING_START", "messageId": "span"}),
        json!({"type": "REASONING_MESSAGE_START", "messageId": "r1", "role": "reasoning"}),
        json!({"type": "TEXT_MESSAGE_CONTENT", "messageId": "m1", "delta": "a"}),
        json!({"type": "TOOL_CALL_START", "toolCallId": "t1", "toolCallName": "f", "parentMessageId": "m1"}),
        json!({"type": "REASONING_MESSAGE_CONTENT", "messageId": "r1", "delta": "hm"}),
        json!({"type": "TOOL_CALL_ARGS", "toolCallId": "t1", "delta": "{}"}),
        json!({"type": "TEXT_MESSAGE_START", "messageId": "m2"}),
        json!({"type": "TEXT_MESSAGE_CONTENT", "messageId": "m1", "delta": "b"}),
        json!({"type": "STEP_FINISHED", "stepName": "think"}),
        json!({"type": "TEXT_MESSAGE_END", "messageId": "m2"}),
        json!({"type": "REASONING_MESSAGE_END", "messageId": "r1"}),
        json!({"type": "TOOL_CALL_END", "toolCallId": "t1"}),
        json!({"type": "REASONING_END", "messageId": "span"}),
        json!({"type": "TEXT_MESSAGE_END", "messageId": "m1"}),
    ]));
    r.assert_ok();
    assert_eq!(r.message("m1")["content"], "ab");

    // Each item still respects its own discipline while interleaved.
    run(&in_run(&[
        json!({"type": "TEXT_MESSAGE_START", "messageId": "m1"}),
        json!({"type": "TOOL_CALL_START", "toolCallId": "t1", "toolCallName": "f"}),
        json!({"type": "TEXT_MESSAGE_END", "messageId": "m1"}),
        json!({"type": "TEXT_MESSAGE_CONTENT", "messageId": "m1", "delta": "late"}),
    ]))
    .assert_rejected("No active text message found with ID 'm1'");
}

/// covers: STRM-06
#[test]
fn standalone_events_appear_anywhere_in_an_open_run() {
    let r = run(&in_run(&[
        json!({"type": "MESSAGES_SNAPSHOT", "messages": [{"id": "u1", "role": "user", "content": "hi"}]}),
        json!({"type": "TEXT_MESSAGE_START", "messageId": "m1"}),
        json!({"type": "STATE_SNAPSHOT", "snapshot": {"n": 1}}),
        json!({"type": "TEXT_MESSAGE_CONTENT", "messageId": "m1", "delta": "a"}),
        json!({"type": "STATE_DELTA", "delta": [{"op": "replace", "path": "/n", "value": 2}]}),
        json!({"type": "ACTIVITY_SNAPSHOT", "messageId": "a1", "activityType": "PLAN", "content": {"s": 1}}),
        json!({"type": "ACTIVITY_DELTA", "messageId": "a1", "activityType": "PLAN", "patch": []}),
        json!({"type": "CUSTOM", "name": "n", "value": 1}),
        json!({"type": "RAW", "event": {"k": 1}}),
        json!({"type": "REASONING_ENCRYPTED_VALUE", "subtype": "message", "entityId": "m1", "encryptedValue": "x"}),
        json!({"type": "TEXT_MESSAGE_CONTENT", "messageId": "m1", "delta": "b"}),
        json!({"type": "TEXT_MESSAGE_END", "messageId": "m1"}),
    ]));
    r.assert_ok();
    assert_eq!(r.state, json!({"n": 2}));
    // Standalone events open and close nothing: the explicit message stayed open.
    assert_eq!(
        r.types()
            .iter()
            .filter(|t| **t == "TEXT_MESSAGE_END")
            .count(),
        1
    );
}
