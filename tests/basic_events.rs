//! Event model (`basic/index.mdx`): the protocol types' half of BASIC-01..19,
//! plus the value-corpus round trip for every valid upstream document.

// Test code: a failed unwrap is a failed test.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;
mod support_pipeline;

use std::collections::HashSet;

use agui_sdk::activity::{ActivityDelta, ActivitySnapshot};
use agui_sdk::capability::{
    AgentCapabilities, ExecutionCapabilities, MultiAgentCapabilities, SubagentInfo,
};
use agui_sdk::content::{Content, PartSource};
use agui_sdk::event::{Event, EventType};
use agui_sdk::ids::{InterruptId, MessageId, RunId, SubagentRunId, ThreadId, ToolCallId};
use agui_sdk::interrupt::{Interrupt, ResumeEntry};
use agui_sdk::json::JsonObject;
use agui_sdk::lifecycle::{
    RunError, RunFinished, RunFinishedOutcome, RunStarted, StepFinished, StepStarted,
};
use agui_sdk::message::{Message, ToolMessage};
use agui_sdk::passthrough::{Custom, Raw};
use agui_sdk::reasoning::{
    EncryptedValueSubtype, ReasoningEncryptedValue, ReasoningEnd, ReasoningMessageChunk,
    ReasoningMessageContent, ReasoningMessageEnd, ReasoningMessageStart, ReasoningRole,
    ReasoningStart,
};
use agui_sdk::run_input::RunAgentInput;
use agui_sdk::state::{MessagesSnapshot, StateDelta, StateSnapshot};
use agui_sdk::subagent::{SubagentError, SubagentFinished, SubagentStarted};
use agui_sdk::text::{TextMessageChunk, TextMessageContent, TextMessageEnd, TextMessageStart};
use agui_sdk::tool::{
    Tool, ToolCallArgs, ToolCallChunk, ToolCallEnd, ToolCallResult, ToolCallStart,
};
use serde::de::DeserializeOwned;
use serde::Serialize;
use serde_json::{json, Value};

/// One event of every type, built through the constructors.
fn one_of_each() -> Vec<Event> {
    vec![
        RunStarted::new("t1", "r1").into(),
        RunFinished::new("t1", "r1").into(),
        RunError::new("boom").into(),
        StepStarted::new("plan").into(),
        StepFinished::new("plan").into(),
        TextMessageStart::new("m1").into(),
        TextMessageContent::new("m1", "hi").into(),
        TextMessageEnd::new("m1").into(),
        TextMessageChunk::new().into(),
        ToolCallStart::new("c1", "search").into(),
        ToolCallArgs::new("c1", "{}").into(),
        ToolCallEnd::new("c1").into(),
        ToolCallChunk::new().into(),
        ToolCallResult::new("m2", "c1", Content::from("ok")).into(),
        StateSnapshot::new(json!({"a": 1})).into(),
        StateDelta::new(Vec::new()).into(),
        MessagesSnapshot::new(Vec::new()).into(),
        ActivitySnapshot::new("a1", "search", JsonObject::new()).into(),
        ActivityDelta::new("a1", "search", Vec::new()).into(),
        ReasoningStart::new("n1").into(),
        ReasoningMessageStart::new("n1", ReasoningRole::Reasoning).into(),
        ReasoningMessageContent::new("n1", "think").into(),
        ReasoningMessageEnd::new("n1").into(),
        ReasoningMessageChunk::new().into(),
        ReasoningEnd::new("n1").into(),
        ReasoningEncryptedValue::new(EncryptedValueSubtype::Message, "n1", "b64").into(),
        SubagentStarted::new("s1", "researcher").into(),
        SubagentFinished::new("s1").into(),
        SubagentError::new("s1", "failed").into(),
        Raw::new(json!({"k": 1})).into(),
        Custom::new("app.x", Value::Null).into(),
    ]
}

fn to_value(event: &Event) -> Value {
    let mut out = Vec::new();
    event.write_json(&mut out);
    serde_json::from_slice(&out).expect("write_json emits JSON")
}

fn parse(json: Value) -> Event {
    serde_json::from_value(json).expect("parse event")
}

fn contains_null(value: &Value) -> bool {
    match value {
        Value::Null => true,
        Value::Array(items) => items.iter().any(contains_null),
        Value::Object(map) => map.values().any(contains_null),
        _ => false,
    }
}

/// covers(client): BASIC-01
/// covers(server): BASIC-01
#[test]
fn every_event_type_round_trips_through_the_typed_model() {
    let events = one_of_each();
    assert_eq!(events.len(), EventType::ALL.len());
    for event in events {
        let mut bytes = Vec::new();
        event.write_json(&mut bytes);
        let back: Event = serde_json::from_slice(&bytes).expect("re-parse");
        assert_eq!(back, event);
    }
}

/// covers(client): BASIC-02
/// covers(server): BASIC-02
#[test]
fn every_event_carries_one_of_the_31_types_first() {
    let schema_types = [
        "TEXT_MESSAGE_START",
        "TEXT_MESSAGE_CONTENT",
        "TEXT_MESSAGE_END",
        "TEXT_MESSAGE_CHUNK",
        "TOOL_CALL_START",
        "TOOL_CALL_ARGS",
        "TOOL_CALL_END",
        "TOOL_CALL_CHUNK",
        "TOOL_CALL_RESULT",
        "STATE_SNAPSHOT",
        "STATE_DELTA",
        "MESSAGES_SNAPSHOT",
        "ACTIVITY_SNAPSHOT",
        "ACTIVITY_DELTA",
        "RAW",
        "CUSTOM",
        "RUN_STARTED",
        "RUN_FINISHED",
        "RUN_ERROR",
        "STEP_STARTED",
        "STEP_FINISHED",
        "REASONING_START",
        "REASONING_MESSAGE_START",
        "REASONING_MESSAGE_CONTENT",
        "REASONING_MESSAGE_END",
        "REASONING_MESSAGE_CHUNK",
        "REASONING_END",
        "REASONING_ENCRYPTED_VALUE",
        "SUBAGENT_STARTED",
        "SUBAGENT_FINISHED",
        "SUBAGENT_ERROR",
    ];
    assert_eq!(EventType::ALL.len(), 31);
    let ours: HashSet<&str> = EventType::ALL.iter().map(|t| t.as_str()).collect();
    assert_eq!(ours, schema_types.iter().copied().collect::<HashSet<_>>());
    for t in EventType::ALL {
        assert_eq!(EventType::parse(t.as_str()), Some(*t));
        assert_eq!(serde_json::to_value(t).unwrap(), json!(t.as_str()));
    }
    assert_eq!(EventType::parse("TEXT_MESSAGE_DELTA"), None);
    assert_eq!(EventType::parse("run_started"), None);

    for event in one_of_each() {
        let mut bytes = Vec::new();
        event.write_json(&mut bytes);
        let expected = format!("{{\"type\":\"{}\"", event.event_type().as_str());
        assert!(
            bytes.starts_with(expected.as_bytes()),
            "{}",
            String::from_utf8_lossy(&bytes)
        );
    }
    // A missing or unknown `type` is not an event.
    assert!(serde_json::from_value::<Event>(json!({"messageId": "m1"})).is_err());
    assert!(serde_json::from_value::<Event>(json!({"type": "NOPE"})).is_err());
}

/// covers(client): BASIC-03
/// covers(server): BASIC-03
#[test]
fn timestamp_is_optional_and_carried_as_sent() {
    let plain = Event::from(TextMessageEnd::new("m1"));
    assert_eq!(plain.timestamp(), None);
    assert!(to_value(&plain).get("timestamp").is_none());

    for ts in [
        0_i64,
        1_700_000_000_000,
        9_007_199_254_740_991,
        -9_007_199_254_740_991,
    ] {
        let event = Event::from(TextMessageEnd::new("m1").with_timestamp(ts));
        assert_eq!(event.timestamp(), Some(ts));
        assert_eq!(to_value(&event)["timestamp"], json!(ts));
    }
    let parsed = parse(json!({"type": "STEP_STARTED", "stepName": "s", "timestamp": 42}));
    assert_eq!(parsed.timestamp(), Some(42));
}

/// covers: BASIC-04
#[test]
fn timestamps_never_reorder_events() {
    // Timestamps that run backwards, through the whole consumer pipeline:
    // delivery and history follow arrival order only.
    let stream = support_pipeline::in_run(&[
        json!({"type": "TEXT_MESSAGE_START", "messageId": "m1", "role": "assistant", "timestamp": 300}),
        json!({"type": "TEXT_MESSAGE_CONTENT", "messageId": "m1", "delta": "a", "timestamp": 100}),
        json!({"type": "TEXT_MESSAGE_CONTENT", "messageId": "m1", "delta": "b", "timestamp": 50}),
        json!({"type": "TEXT_MESSAGE_END", "messageId": "m1", "timestamp": 200}),
        json!({"type": "TEXT_MESSAGE_START", "messageId": "m2", "role": "assistant", "timestamp": 10}),
        json!({"type": "TEXT_MESSAGE_CONTENT", "messageId": "m2", "delta": "c", "timestamp": 5}),
        json!({"type": "TEXT_MESSAGE_END", "messageId": "m2", "timestamp": 1}),
    ]);
    for r in [
        support_pipeline::run(&stream),
        support_pipeline::run_value(&stream),
    ] {
        r.assert_ok();
        let stamps: Vec<_> = r.events.iter().map(Event::timestamp).collect();
        assert_eq!(
            stamps,
            [
                None,
                Some(300),
                Some(100),
                Some(50),
                Some(200),
                Some(10),
                Some(5),
                Some(1),
                None
            ]
        );
        assert_eq!(
            r.messages,
            [
                json!({"id": "m1", "role": "assistant", "content": "ab"}),
                json!({"id": "m2", "role": "assistant", "content": "c"}),
            ]
        );
    }
}

/// covers(client): BASIC-05
/// covers(server): BASIC-05
#[test]
fn raw_event_is_carried_verbatim() {
    for raw in [
        json!({"provider": {"chunk": [1, null, "x"]}}),
        json!([1, 2]),
        json!("text"),
        json!(42),
    ] {
        let event =
            parse(json!({"type": "TEXT_MESSAGE_END", "messageId": "m1", "rawEvent": raw.clone()}));
        assert_eq!(event.raw_event(), Some(&raw));
        assert_eq!(to_value(&event)["rawEvent"], raw);
    }
}

/// covers: BASIC-06
#[test]
fn raw_event_never_drives_protocol_behaviour() {
    let event = parse(json!({
        "type": "TEXT_MESSAGE_END",
        "messageId": "m1",
        "rawEvent": {"type": "RUN_ERROR", "subagentRunId": "s9", "messageId": "other"}
    }));
    assert_eq!(event.event_type(), EventType::TextMessageEnd);
    assert!(!event.is_run_terminal());
    assert_eq!(event.subagent_run_id(), None);
    let Event::TextMessageEnd(end) = event else {
        panic!("variant")
    };
    assert_eq!(end.message_id, *"m1");
}

/// covers(client): BASIC-07
/// covers(server): BASIC-07
#[test]
fn metadata_and_attribution_ride_on_events() {
    let mut meta = JsonObject::new();
    meta.insert("acme.trace", "t-1");
    let event = Event::from(
        TextMessageContent::new("m1", "hi")
            .with_metadata(meta.clone())
            .with_subagent_run_id("s1"),
    );
    assert_eq!(event.metadata(), Some(&meta));
    assert_eq!(
        event.subagent_run_id().map(SubagentRunId::as_str),
        Some("s1")
    );
    let value = to_value(&event);
    assert_eq!(value["metadata"], json!({"acme.trace": "t-1"}));
    assert_eq!(value["subagentRunId"], json!("s1"));

    let parsed = parse(
        json!({"type": "TOOL_CALL_END", "toolCallId": "c1", "subagentRunId": "s2", "metadata": {"k": 1}}),
    );
    assert_eq!(
        parsed.subagent_run_id().map(SubagentRunId::as_str),
        Some("s2")
    );
    assert_eq!(parsed.metadata().and_then(|m| m.get("k")), Some(&json!(1)));
}

/// covers(client): BASIC-08
/// covers(server): BASIC-08
#[test]
fn run_scoped_events_carry_no_attribution() {
    let scoped: Vec<EventType> = EventType::ALL
        .iter()
        .copied()
        .filter(|t| t.is_run_scoped())
        .collect();
    assert_eq!(
        scoped,
        [
            EventType::RunStarted,
            EventType::RunFinished,
            EventType::RunError,
            EventType::MessagesSnapshot
        ]
    );
    for event in one_of_each() {
        if event.event_type().is_run_scoped() {
            assert_eq!(event.subagent_run_id(), None);
            assert!(to_value(&event).get("subagentRunId").is_none());
        }
    }
    // The typed model has nowhere to keep a stray attribution on them.
    let parsed = parse(
        json!({"type": "RUN_FINISHED", "threadId": "t", "runId": "r", "subagentRunId": "s1"}),
    );
    assert_eq!(parsed.subagent_run_id(), None);
    assert!(to_value(&parsed).get("subagentRunId").is_none());
}

/// covers(client): BASIC-09
/// covers(server): BASIC-09
#[test]
fn optional_fields_without_a_value_are_omitted_not_null() {
    // A `null` open-JSON value is absence: setters normalise it, and a field
    // set to `Some(Null)` directly is still not written.
    let via_setter = Event::from(TextMessageEnd::new("m").with_raw_event(Value::Null));
    assert_eq!(via_setter.raw_event(), None);
    let mut direct = RunFinished::new("t", "r");
    direct.result = Some(Value::Null);
    direct.raw_event = Some(Value::Null);
    assert_eq!(
        to_value(&direct.into()),
        json!({"type": "RUN_FINISHED", "threadId": "t", "runId": "r"})
    );
    let mut input = RunAgentInput::new("t", "r", Vec::new()).with_state(Value::Null);
    input.forwarded_props = Some(Value::Null);
    input.tools = Some(Vec::new());
    assert_eq!(
        serde_json::to_value(&input).unwrap(),
        json!({"threadId": "t", "runId": "r", "messages": [], "tools": []}),
        "a present empty list is kept; null open JSON is not written"
    );
    for event in one_of_each() {
        let value = to_value(&event);
        let allowed_null = matches!(event, Event::Custom(_));
        if !allowed_null {
            assert!(!contains_null(&value), "{value}");
        }
    }
    let finished = to_value(&RunFinished::new("t", "r").into());
    assert_eq!(
        finished,
        json!({"type": "RUN_FINISHED", "threadId": "t", "runId": "r"})
    );
    let input = serde_json::to_value(RunAgentInput::new("t", "r", Vec::new())).unwrap();
    assert_eq!(
        input,
        json!({"threadId": "t", "runId": "r", "messages": []})
    );
}

/// covers(client): BASIC-10
/// covers(server): BASIC-10
#[test]
fn required_json_payloads_may_be_null() {
    let custom = parse(json!({"type": "CUSTOM", "name": "x", "value": null}));
    assert_eq!(
        to_value(&custom),
        json!({"type": "CUSTOM", "name": "x", "value": null})
    );
    let snapshot = parse(json!({"type": "STATE_SNAPSHOT", "snapshot": null}));
    assert_eq!(
        to_value(&snapshot),
        json!({"type": "STATE_SNAPSHOT", "snapshot": null})
    );
    // Absent is not null: the payload is required.
    assert!(serde_json::from_value::<Event>(json!({"type": "CUSTOM", "name": "x"})).is_err());
    assert!(serde_json::from_value::<Event>(json!({"type": "STATE_SNAPSHOT"})).is_err());
    assert!(serde_json::from_value::<Event>(json!({"type": "RAW"})).is_err());
}

/// covers: BASIC-11
#[test]
fn null_under_open_objects_is_preserved() {
    let doc = json!({
        "type": "ACTIVITY_SNAPSHOT", "messageId": "a1", "activityType": "x",
        "content": {"progress": null, "nested": {"n": null}},
        "metadata": {"finishReason": null}
    });
    let event = parse(doc.clone());
    assert_eq!(
        event.metadata().and_then(|m| m.get("finishReason")),
        Some(&Value::Null)
    );
    assert_eq!(to_value(&event), doc);
    let state = json!({"type": "STATE_SNAPSHOT", "snapshot": {"a": null, "b": [null]}});
    assert_eq!(to_value(&parse(state.clone())), state);
}

/// covers: BASIC-12
#[test]
fn tolerated_optional_null_is_never_re_emitted() {
    // The typed model reads a stray `null` in an optional position as absent
    // (the compatibility boundary warns about it upstream of here); what it
    // writes back is the conformant form: the member omitted.
    let event = parse(
        json!({"type": "TEXT_MESSAGE_START", "messageId": "m1", "name": null, "rawEvent": null}),
    );
    assert_eq!(
        to_value(&event),
        json!({"type": "TEXT_MESSAGE_START", "messageId": "m1"})
    );
}

/// covers: BASIC-13
#[test]
fn thread_ids_are_application_minted_and_stable() {
    let thread = ThreadId::new("conversation-42");
    let first = RunAgentInput::new(thread.clone(), RunId::random(), Vec::new());
    let second = RunAgentInput::new(thread.clone(), RunId::random(), Vec::new());
    assert_eq!(first.thread_id, second.thread_id);
    assert_eq!(first.thread_id.as_str(), "conversation-42");
    // Clones share storage: no allocation, same text.
    assert!(std::sync::Arc::ptr_eq(
        thread.as_arc(),
        first.thread_id.as_arc()
    ));
    let random = ThreadId::random();
    assert_eq!(random.as_str().len(), 32);
    assert!(random
        .as_str()
        .bytes()
        .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase()));
}

/// covers(client): BASIC-14
/// covers(server): BASIC-14
#[test]
fn minted_run_ids_are_never_reused() {
    let ids: HashSet<RunId> = (0..10_000).map(|_| RunId::random()).collect();
    assert_eq!(ids.len(), 10_000);
    let threads: Vec<_> = (0..4)
        .map(|_| std::thread::spawn(|| (0..1000).map(|_| RunId::random()).collect::<Vec<_>>()))
        .collect();
    let mut all = ids;
    for handle in threads {
        for id in handle.join().unwrap() {
            assert!(all.insert(id));
        }
    }
}

/// covers: BASIC-15
#[test]
fn minted_message_ids_are_unique() {
    let ids: HashSet<MessageId> = (0..2000)
        .flat_map(|_| {
            [
                Message::user("a"),
                Message::assistant("b"),
                Message::tool("c1", "r"),
            ]
        })
        .map(|m| m.id().clone())
        .collect();
    assert_eq!(ids.len(), 6000);
}

/// A `MESSAGES_SNAPSHOT` puts its message ids into the thread, so a later
/// `TOOL_CALL_RESULT` cannot mint a tool message under one of them and
/// silently replace the held message: the same verdict as for an id a
/// streamed message took.
///
/// covers: BASIC-15
#[test]
fn a_snapshot_message_id_cannot_be_reused_by_a_tool_result() {
    use agui_sdk::diagnostics::{NullSink, Reporter};
    use agui_sdk::pipeline::{PipelineConfig, Processor};
    use agui_sdk::ErrorCode;
    use std::sync::Arc;

    let result = json!({"type": "TOOL_CALL_RESULT", "messageId": "u1", "toolCallId": "c1",
                        "content": "done"});
    for opener in [
        json!({"type": "MESSAGES_SNAPSHOT", "messages": [
            {"id": "u1", "role": "user", "content": "hello"}]}),
        json!({"type": "TEXT_MESSAGE_START", "messageId": "u1", "role": "user"}),
    ] {
        let mut frames = vec![
            json!({"type": "RUN_STARTED", "threadId": "t", "runId": "r"}),
            opener.clone(),
        ];
        if opener["type"] == "TEXT_MESSAGE_START" {
            frames.push(json!({"type": "TEXT_MESSAGE_END", "messageId": "u1"}));
        }
        frames.push(result.clone());
        let mut processor = Processor::new(PipelineConfig::default());
        let mut reporter = Reporter::new(Arc::new(NullSink), 256);
        let mut out = Vec::new();
        let verdict = frames.iter().try_for_each(|frame| {
            let bytes = serde_json::to_vec(frame).unwrap();
            processor.push_json(&bytes, &mut reporter, &mut out)
        });
        let err = verdict.expect_err("the reused id is refused");
        assert_eq!(err.code(), ErrorCode::Identity, "after {opener}");
        // The held message is untouched.
        let held = processor.reducer().messages();
        assert_eq!(held.len(), 1);
        assert_eq!(held[0].role(), agui_sdk::message::Role::User);
    }
}

/// covers: BASIC-16
#[test]
fn snapshot_messages_keep_their_ids() {
    let event = parse(json!({"type": "MESSAGES_SNAPSHOT", "messages": [
        {"id": "m1", "role": "assistant", "content": "restated"},
        {"role": "user", "id": "m2", "content": "hello"}
    ]}));
    let Event::MessagesSnapshot(snapshot) = event else {
        panic!("variant")
    };
    let ids: Vec<&str> = snapshot.messages.iter().map(|m| m.id().as_str()).collect();
    assert_eq!(ids, ["m1", "m2"]);
    assert_eq!(snapshot.messages[0].text(), Some("restated"));
}

/// covers(client): BASIC-17
/// covers(server): BASIC-17
#[test]
fn tool_call_id_ties_call_result_and_interrupt() {
    let call = ToolCallId::new("call-7");
    let result = ToolCallResult::new("m9", call.clone(), Content::from("done"));
    let answer = ToolMessage::new("m10", Content::from("done"), call.clone());
    let interrupt = Interrupt::new("i1", "tool_approval").with_tool_call_id(call.clone());
    assert_eq!(result.tool_call_id, call);
    assert_eq!(answer.tool_call_id, call);
    assert_eq!(interrupt.tool_call_id.as_ref(), Some(&call));
    let start =
        parse(json!({"type": "TOOL_CALL_START", "toolCallId": "call-7", "toolCallName": "x"}));
    let Event::ToolCallStart(start) = start else {
        panic!("variant")
    };
    assert_eq!(start.tool_call_id, call);
}

/// covers: BASIC-18
#[test]
fn subagent_run_ids_are_per_invocation_handles() {
    let a = SubagentRunId::random();
    let b = SubagentRunId::random();
    assert_ne!(a, b);
    let started_a = Event::from(SubagentStarted::new(a.clone(), "researcher"));
    let started_b = Event::from(SubagentStarted::new(b.clone(), "researcher"));
    assert_eq!(started_a.subagent_run_id(), Some(&a));
    assert_eq!(started_b.subagent_run_id(), Some(&b));
}

/// covers: BASIC-19
#[test]
fn identifiers_are_opaque_strings() {
    for raw in [
        "",
        "a/b:c?d#e",
        "run 1",
        "😀-id",
        "{\"json\":true}",
        "00000000-0000-0000-0000-000000000000",
    ] {
        let id = MessageId::new(raw);
        assert_eq!(id.as_str(), raw);
        assert_eq!(id.to_string(), raw);
        assert_eq!(serde_json::to_value(&id).unwrap(), json!(raw));
        let back: MessageId = serde_json::from_value(json!(raw)).unwrap();
        assert_eq!(back, id);
        assert!(id == *raw);
        let _: &InterruptId = &InterruptId::from(raw);
    }
    assert_eq!(format!("{:?}", RunId::new("r1")), "RunId(\"r1\")");
}

/// Debug output hides encrypted values and reasoning text.
///
/// covers(client): BASIC-01
#[test]
fn debug_redacts_encrypted_values_and_reasoning() {
    let secret = "SECRET-CIPHERTEXT";
    let shown = [
        format!(
            "{:?}",
            Event::from(ReasoningEncryptedValue::new(
                EncryptedValueSubtype::ToolCall,
                "c1",
                secret
            ))
        ),
        format!(
            "{:?}",
            Event::from(ReasoningMessageContent::new("n1", secret))
        ),
        format!(
            "{:?}",
            Event::from(ReasoningMessageChunk::new().with_delta(secret))
        ),
        // A provider-native event on any reasoning event can carry the
        // summary or signature, so `raw_event` is redacted on all of them.
        format!(
            "{:?}",
            Event::from(ReasoningStart::new("s1").with_raw_event(json!({"text": secret})))
        ),
        format!(
            "{:?}",
            Event::from(
                ReasoningMessageStart::new("n1", ReasoningRole::Reasoning)
                    .with_raw_event(json!({"text": secret}))
            )
        ),
        format!(
            "{:?}",
            Event::from(ReasoningMessageEnd::new("n1").with_raw_event(json!({"text": secret})))
        ),
        format!(
            "{:?}",
            Event::from(ReasoningEnd::new("s1").with_raw_event(json!({"text": secret})))
        ),
        format!(
            "{:?}",
            parse(json!({"type": "MESSAGES_SNAPSHOT", "messages": [
                {"id": "1", "role": "reasoning", "content": secret, "encryptedValue": secret},
                {"id": "2", "role": "assistant", "encryptedValue": secret,
                 "toolCalls": [{"id": "c", "type": "function", "function": {"name": "f", "arguments": "{}"}, "encryptedValue": secret}]},
                {"id": "3", "role": "user", "content": "hi", "encryptedValue": secret},
                {"id": "4", "role": "developer", "content": "hi", "encryptedValue": secret},
                {"id": "5", "role": "system", "content": "hi", "encryptedValue": secret},
                {"id": "6", "role": "tool", "content": "hi", "toolCallId": "c", "encryptedValue": secret}
            ]}))
        ),
    ];
    for text in shown {
        assert!(!text.contains(secret), "{text}");
        assert!(text.contains("<redacted>"), "{text}");
    }
}

/// Parsing tolerates any member order: a tag after other members is read
/// the same as a tag in first position.
///
/// covers(client): BASIC-02
#[test]
fn tag_position_does_not_matter() {
    let first: Event = serde_json::from_str(
        r#"{"type":"TOOL_CALL_RESULT","messageId":"m","toolCallId":"c","content":[{"type":"text","text":"x"}]}"#,
    )
    .unwrap();
    let last: Event = serde_json::from_str(
        r#"{"messageId":"m","toolCallId":"c","content":[{"text":"x","type":"text"}],"type":"TOOL_CALL_RESULT"}"#,
    )
    .unwrap();
    assert_eq!(first, last);
    let outcome: RunFinishedOutcome =
        serde_json::from_str(r#"{"interrupts":[{"reason":"r","id":"i"}],"type":"interrupt"}"#)
            .unwrap();
    assert_eq!(
        outcome,
        RunFinishedOutcome::interrupt([Interrupt::new("i", "r")])
    );
}

// ---- value corpus --------------------------------------------------------

fn round_trip<T: Serialize + DeserializeOwned>(bytes: &[u8]) -> Result<Value, String> {
    let typed: T = serde_json::from_slice(bytes).map_err(|e| format!("parse: {e}"))?;
    serde_json::to_value(&typed).map_err(|e| format!("serialize: {e}"))
}

fn round_trip_event(bytes: &[u8]) -> Result<Value, String> {
    let event: Event = serde_json::from_slice(bytes).map_err(|e| format!("parse: {e}"))?;
    let mut out = Vec::new();
    event.write_json(&mut out);
    serde_json::from_slice(&out).map_err(|e| format!("reparse: {e}"))
}

/// Every valid document of the upstream value corpus parses into its typed
/// form and re-serializes to a semantically equal document, with optional
/// members omitted rather than `null`.
///
/// covers(client): BASIC-01, BASIC-09
/// covers(server): BASIC-01, BASIC-09
#[test]
fn value_corpus_valid_documents_round_trip() {
    let Some(documents) = support::value_documents(true) else {
        return;
    };
    let mut failures = Vec::new();
    let mut checked = 0;
    for doc in &documents {
        let anchor = doc.anchor.as_str();
        let result = match anchor {
            a if a.ends_with("Event") => round_trip_event(&doc.bytes),
            "AgentCapabilities" => round_trip::<AgentCapabilities>(&doc.bytes),
            "ExecutionCapabilities" => round_trip::<ExecutionCapabilities>(&doc.bytes),
            "MultiAgentCapabilities" => round_trip::<MultiAgentCapabilities>(&doc.bytes),
            "SubagentInfo" => round_trip::<SubagentInfo>(&doc.bytes),
            "Tool" => round_trip::<Tool>(&doc.bytes),
            "Interrupt" => round_trip::<Interrupt>(&doc.bytes),
            "ResumeEntry" => round_trip::<ResumeEntry>(&doc.bytes),
            "RunAgentInput" => round_trip::<RunAgentInput>(&doc.bytes),
            // These anchors' documents carry their discriminator.
            "FileSource" => round_trip::<PartSource>(&doc.bytes),
            "ToolMessage" | "UserMessage" => round_trip::<Message>(&doc.bytes),
            other => panic!("no typed mapping for anchor {other} ({})", doc.relative),
        };
        let original: Value = serde_json::from_slice(&doc.bytes).expect("fixture is JSON");
        match result {
            Ok(back) if back == original => checked += 1,
            Ok(back) => failures.push(format!("{}: {back} != {original}", doc.relative)),
            Err(e) => failures.push(format!("{}: {e}", doc.relative)),
        }
    }
    eprintln!("round-tripped {checked} valid documents");
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}
