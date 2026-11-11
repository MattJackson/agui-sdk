//! `REASONING_ENCRYPTED_VALUE` and reasoning messages in history
//! (`events/reasoning.mdx`).

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support_pipeline;
mod support_reducer;

use agui_sdk::diagnostics::DiagnosticCode;
use agui_sdk::Event;
use serde_json::json;
use support_reducer::Harness;

fn encrypted(subtype: &str, entity: &str, value: &str) -> serde_json::Value {
    json!({"type": "REASONING_ENCRYPTED_VALUE", "subtype": subtype, "entityId": entity, "encryptedValue": value})
}

fn with_message_and_call() -> Harness {
    let mut h = Harness::new();
    h.push(json!({"type": "REASONING_START", "messageId": "s1"}));
    h.push(json!({"type": "REASONING_MESSAGE_START", "messageId": "r1", "role": "reasoning"}));
    h.push(json!({"type": "REASONING_MESSAGE_CONTENT", "messageId": "r1", "delta": "think"}));
    h.push(json!({"type": "REASONING_MESSAGE_END", "messageId": "r1"}));
    h.push(json!({"type": "REASONING_END", "messageId": "s1"}));
    h.push(json!({"type": "TOOL_CALL_START", "toolCallId": "c1", "toolCallName": "f", "parentMessageId": "a1"}));
    h.push(json!({"type": "TOOL_CALL_END", "toolCallId": "c1"}));
    h
}

/// covers: REAS-14, REAS-15
#[test]
fn value_is_stored_opaque_on_the_named_message_or_call() {
    let mut h = with_message_and_call();
    // Not JSON, not base64: stored byte for byte.
    let odd = "{not json ✓ \u{0}~/";
    h.push(encrypted("message", "r1", odd));
    h.push(encrypted("tool-call", "c1", "opaque-call"));
    let history = h.history();
    assert_eq!(history[0]["role"], "reasoning");
    assert_eq!(history[0]["content"], "think");
    assert_eq!(history[0]["encryptedValue"], odd);
    assert_eq!(history[1]["toolCalls"][0]["encryptedValue"], "opaque-call");
    assert!(
        history[1].get("encryptedValue").is_none(),
        "the call's value is not the message's"
    );
    assert!(h.codes().is_empty());
}

/// covers: REAS-16
#[test]
fn naming_an_activity_message_is_the_unknown_entity_case() {
    let mut h = Harness::new();
    h.push(json!({"type": "ACTIVITY_SNAPSHOT", "messageId": "act", "activityType": "s", "content": {}}));
    let before = h.history();
    h.push(encrypted("message", "act", "v"));
    assert_eq!(h.history(), before);
    assert_eq!(h.codes(), ["reasoning.unknown_entity"]);
}

/// covers: REAS-20, REAS-21, REAS-22
#[test]
fn unknown_entity_warns_and_is_dropped() {
    let mut h = with_message_and_call();
    let before = h.history();
    h.push(encrypted("message", "ghost", "v"));
    h.push(encrypted("tool-call", "r1", "v"));
    assert_eq!(h.history(), before);
    let d = h.take();
    assert_eq!(d.len(), 2);
    assert!(d
        .iter()
        .all(|d| d.code == DiagnosticCode::UnknownEncryptedEntity));
    assert_eq!(d[0].pointer, "/entityId");
    assert!(!d[0].message.contains("ghost"));
    // The run continues.
    h.push(encrypted("message", "r1", "late"));
    assert_eq!(h.history()[0]["encryptedValue"], "late");
}

/// covers: REAS-19
#[test]
fn value_is_standalone_and_opens_nothing() {
    let mut h = Harness::new();
    h.push(json!({"type": "TEXT_MESSAGE_START", "messageId": "m1"}));
    h.push(encrypted("message", "m1", "v"));
    h.push(json!({"type": "TEXT_MESSAGE_CONTENT", "messageId": "m1", "delta": "after"}));
    let history = h.history();
    assert_eq!(history.as_array().unwrap().len(), 1);
    assert_eq!(history[0]["content"], "after");
    assert_eq!(history[0]["encryptedValue"], "v");
}

/// covers: REAS-17, REAS-25
#[test]
fn value_is_preserved_into_the_next_input_and_nowhere_else() {
    let mut h = with_message_and_call();
    h.push(encrypted("message", "r1", "keep-me"));
    let (messages, _) = h.reducer.into_parts();
    // Returned unmodified with its message for the next run's input...
    let json = serde_json::to_value(&messages).unwrap();
    assert_eq!(json[0]["encryptedValue"], "keep-me");
    // ...and nothing was lost on the way, so there is nothing to warn about.
    let again: Vec<agui_sdk::Message> = serde_json::from_value(json.clone()).unwrap();
    assert_eq!(serde_json::to_value(again).unwrap(), json);
}

/// covers: REAS-23
#[test]
fn malformed_reasoning_events_cannot_be_typed() {
    for bad in [
        json!({"type": "REASONING_ENCRYPTED_VALUE", "subtype": 1, "entityId": "e", "encryptedValue": "v"}),
        json!({"type": "REASONING_ENCRYPTED_VALUE", "subtype": "message", "encryptedValue": "v"}),
        // An unnamed string subtype is stripped by enforcement with its event;
        // it never reaches the typed path.
        json!({"type": "REASONING_ENCRYPTED_VALUE", "subtype": "thought", "entityId": "e", "encryptedValue": "v"}),
    ] {
        assert!(
            serde_json::from_value::<Event>(bad.clone()).is_err(),
            "{bad}"
        );
    }

    // Through the consumer pipeline, on both entry paths: the malformed
    // ones are fatal; the unnamed string subtype is unrecognised material in
    // a required position, so its event is dropped with a warning and the
    // run goes on.
    for bad in [
        json!({"type": "REASONING_ENCRYPTED_VALUE", "subtype": 1, "entityId": "e", "encryptedValue": "v"}),
        json!({"type": "REASONING_ENCRYPTED_VALUE", "subtype": "message", "encryptedValue": "v"}),
        json!({"type": "REASONING_ENCRYPTED_VALUE", "subtype": "message", "entityId": "e"}),
    ] {
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
        }
    }
    let stream = support_pipeline::in_run(&[encrypted("thought", "e", "v")]);
    for r in [
        support_pipeline::run(&stream),
        support_pipeline::run_value(&stream),
    ] {
        r.assert_ok();
        assert_eq!(r.types(), ["RUN_STARTED", "RUN_FINISHED"]);
        assert!(
            r.warned(DiagnosticCode::UnknownEnumValue),
            "{:?}",
            r.diagnostics
        );
    }
}

/// covers: REAS-24
#[test]
fn reasoning_is_kept_in_history_like_the_conversation() {
    let mut h = with_message_and_call();
    h.push(json!({"type": "MESSAGES_SNAPSHOT", "messages": [
        {"id": "a1", "role": "assistant", "toolCalls": [
            {"id": "c1", "type": "function", "function": {"name": "f", "arguments": ""}}]}
    ]}));
    // Kept, as part of the conversation, through a snapshot that omits it.
    assert_eq!(h.history()[0]["id"], "r1");
    assert_eq!(h.history()[0]["role"], "reasoning");
}

/// covers: REAS-26
#[test]
fn snapshot_restating_a_message_replaces_its_value() {
    let mut h = Harness::new();
    h.push(json!({"type": "TEXT_MESSAGE_START", "messageId": "a1"}));
    h.push(json!({"type": "TEXT_MESSAGE_END", "messageId": "a1"}));
    h.push(encrypted("message", "a1", "old"));
    h.push(json!({"type": "MESSAGES_SNAPSHOT", "messages": [
        {"id": "a1", "role": "assistant", "content": "restated"}
    ]}));
    assert!(
        h.history()[0].get("encryptedValue").is_none(),
        "withdrawn, not resurrected"
    );
    h.push(json!({"type": "MESSAGES_SNAPSHOT", "messages": [
        {"id": "a1", "role": "assistant", "content": "restated", "encryptedValue": "new"}
    ]}));
    assert_eq!(h.history()[0]["encryptedValue"], "new");
}
