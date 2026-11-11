//! Metadata, producer side (`basic/metadata.mdx`).

// Test code: a failed unwrap is a failed test.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use agui_sdk::content::Content;
use agui_sdk::diagnostics::DiagnosticCode;
use agui_sdk::event::Event;
use agui_sdk::interrupt::{Interrupt, ResumeEntry, ResumeStatus};
use agui_sdk::message::{AssistantMessage, Message};
use agui_sdk::metadata::{self, Metadata, MetadataBuilder, RESERVED_KEY};
use agui_sdk::text::TextMessageContent;
use agui_sdk::tool::{FunctionCall, Tool, ToolCall, ToolCallResult, ToolCallType};
use serde_json::{json, Value};

fn sample() -> Metadata {
    MetadataBuilder::vendor("acme")
        .insert("trace", "t-1")
        .build()
}

/// covers: META-01
#[test]
fn metadata_is_open_by_key_and_keeps_null() {
    let meta = MetadataBuilder::new()
        .insert("finishReason", Value::Null)
        .insert("nested", json!({"a": [1, null]}))
        .insert("count", 3)
        .build();
    assert_eq!(meta.get("finishReason"), Some(&Value::Null));
    assert!(meta.contains_key("finishReason"));
    assert_eq!(
        serde_json::to_value(&meta).unwrap(),
        json!({"finishReason": null, "nested": {"a": [1, null]}, "count": 3})
    );
}

/// covers: META-02
#[test]
fn the_metadata_object_itself_is_never_null() {
    let without = Event::from(TextMessageContent::new("m", "x"));
    let mut out = Vec::new();
    without.write_json(&mut out);
    assert_eq!(
        out,
        br#"{"type":"TEXT_MESSAGE_CONTENT","messageId":"m","delta":"x"}"#
    );

    let empty = Event::from(
        TextMessageContent::new("m", "x").with_metadata(MetadataBuilder::new().build()),
    );
    empty.write_json(&mut out);
    assert_eq!(
        out,
        br#"{"type":"TEXT_MESSAGE_CONTENT","messageId":"m","delta":"x","metadata":{}}"#
    );
}

/// covers: META-04
#[test]
fn the_reserved_key_is_refused() {
    let meta = MetadataBuilder::new()
        .insert(RESERVED_KEY, json!({"origin": "me"}))
        .insert("ok", 1)
        .build();
    assert!(!meta.contains_key(RESERVED_KEY));
    assert_eq!(meta.len(), 1);
    // A vendor prefix can never produce the reserved key.
    let prefixed = MetadataBuilder::vendor("acme")
        .insert(RESERVED_KEY, 1)
        .build();
    assert!(prefixed.contains_key("acme.ag-ui"));
}

/// covers: META-04
#[test]
fn try_insert_returns_the_refusal_for_the_caller_to_report() {
    let refused = MetadataBuilder::new()
        .try_insert(RESERVED_KEY, 1)
        .expect_err("reserved key refused");
    assert_eq!(refused.code, DiagnosticCode::ReservedMetadataKey);
    assert_eq!(refused.pointer, "/ag-ui");
    let accepted = MetadataBuilder::vendor("acme")
        .try_insert("trace", "t-1")
        .expect("ordinary key accepted")
        .build();
    assert!(accepted.contains_key("acme.trace"));
}

/// covers: META-05
#[test]
fn invented_keys_get_a_vendor_prefix() {
    assert_eq!(metadata::vendor_key("acme", "trace_id"), "acme.trace_id");
    let meta = MetadataBuilder::vendor("acme")
        .insert("trace_id", "t-1")
        .insert("tier", 2)
        .build();
    let keys: Vec<&str> = meta.iter().map(|(k, _)| k).collect();
    assert!(keys.iter().all(|k| k.starts_with("acme.")), "{keys:?}");
    assert_eq!(metadata::provider::normalize(" Anthropic "), "anthropic");
    assert!(matches!(
        metadata::provider::normalize("openai"),
        std::borrow::Cow::Borrowed("openai")
    ));
}

/// covers: META-17
#[test]
fn metadata_rides_on_every_carrier() {
    let expected = json!({"acme.trace": "t-1"});
    let event =
        Event::from(ToolCallResult::new("m1", "c1", Content::from("ok")).with_metadata(sample()));
    assert_eq!(event.metadata(), Some(&sample()));

    let call = ToolCall::new("c1", ToolCallType::Function, FunctionCall::new("f", "{}"))
        .with_metadata(sample());
    let message = Message::from(
        AssistantMessage::new("m2")
            .with_tool_calls([call])
            .with_metadata(sample()),
    );
    let value = serde_json::to_value(&message).unwrap();
    assert_eq!(value["metadata"], expected);
    assert_eq!(value["toolCalls"][0]["metadata"], expected);
    assert_eq!(message.metadata(), Some(&sample()));

    let tool = serde_json::to_value(Tool::new("f", "d").with_metadata(sample())).unwrap();
    assert_eq!(tool["metadata"], expected);
    let interrupt = serde_json::to_value(Interrupt::new("i", "r").with_metadata(sample())).unwrap();
    assert_eq!(interrupt["metadata"], expected);
    let entry =
        serde_json::to_value(ResumeEntry::new("i", ResumeStatus::Resolved).with_metadata(sample()))
            .unwrap();
    assert_eq!(entry["metadata"], expected);

    // And comes back intact.
    let back: Message = serde_json::from_value(value).unwrap();
    assert_eq!(back, message);
}
