//! Tool calls, consumer side (`events/tool-calls.mdx`).

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support_pipeline;

use std::sync::Arc;

use agui_sdk::content::Content;
use agui_sdk::diagnostics::{CollectingSink, Reporter};
use agui_sdk::pipeline::compat::EraShims;
use agui_sdk::pipeline::{PipelineConfig, Processor};
use agui_sdk::version::LegacyEra;
use agui_sdk::ErrorCode;
use serde_json::{json, Value};
use support_pipeline::{in_run, run, started, text};

fn call(id: &str, name: &str, args: &[&str]) -> Vec<Value> {
    let mut events = vec![
        json!({"type": "TOOL_CALL_START", "toolCallId": id, "toolCallName": name, "parentMessageId": "m1"}),
    ];
    for delta in args {
        events.push(json!({"type": "TOOL_CALL_ARGS", "toolCallId": id, "delta": delta}));
    }
    events.push(json!({"type": "TOOL_CALL_END", "toolCallId": id}));
    events
}

fn with_message(calls: &[Value]) -> Vec<Value> {
    let mut body = text("m1", "Looking it up.").to_vec();
    body.extend_from_slice(calls);
    in_run(&body)
}

/// covers: TOOL-01, TOOL-03
#[test]
fn a_call_attaches_to_its_message_and_arguments_concatenate() {
    let r = run(&with_message(&call(
        "t1",
        "search",
        &["{\"q\":", "\"agui\"}"],
    )));
    r.assert_ok();
    let call = &r.message("m1")["toolCalls"][0];
    assert_eq!(call["id"], "t1");
    assert_eq!(call["function"]["name"], "search");
    assert_eq!(call["function"]["arguments"], "{\"q\":\"agui\"}");
}

/// covers: TOOL-04
#[test]
fn arguments_are_not_offered_for_action_before_the_call_closes() {
    let mut processor = Processor::new(PipelineConfig::default());
    let mut reporter = Reporter::new(Arc::new(CollectingSink::new()), 256);
    let mut out = Vec::new();
    let mut body = text("m1", "x").to_vec();
    body.insert(0, started("r1"));
    body.push(json!({"type": "TOOL_CALL_START", "toolCallId": "t1", "toolCallName": "confirm", "parentMessageId": "m1"}));
    body.push(json!({"type": "TOOL_CALL_ARGS", "toolCallId": "t1", "delta": "{\"ok\":"}));
    for frame in &body {
        processor
            .push_json(&serde_json::to_vec(frame).unwrap(), &mut reporter, &mut out)
            .unwrap();
    }
    assert!(processor.reducer().pending_tool_calls().is_empty());
    for frame in [
        json!({"type": "TOOL_CALL_ARGS", "toolCallId": "t1", "delta": "true}"}),
        json!({"type": "TOOL_CALL_END", "toolCallId": "t1"}),
        json!({"type": "RUN_FINISHED", "threadId": "t", "runId": "r1"}),
    ] {
        processor
            .push_json(
                &serde_json::to_vec(&frame).unwrap(),
                &mut reporter,
                &mut out,
            )
            .unwrap();
    }
    let pending = processor.reducer().pending_tool_calls();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].arguments, "{\"ok\":true}");

    // A call the run never closed is not offered, even once the run is over:
    // RUN_ERROR cuts `t2` off mid-arguments.
    let mut processor = Processor::new(PipelineConfig::default());
    let mut out = Vec::new();
    let mut body = text("m1", "x").to_vec();
    body.insert(0, started("r1"));
    body.push(json!({"type": "TOOL_CALL_START", "toolCallId": "t2", "toolCallName": "transfer", "parentMessageId": "m1"}));
    body.push(json!({"type": "TOOL_CALL_ARGS", "toolCallId": "t2", "delta": "{\"amount\":10"}));
    body.push(json!({"type": "RUN_ERROR", "message": "provider went away"}));
    for frame in &body {
        processor
            .push_json(&serde_json::to_vec(frame).unwrap(), &mut reporter, &mut out)
            .unwrap();
    }
    let ids: Vec<&str> = processor
        .reducer()
        .pending_tool_calls()
        .iter()
        .map(|p| p.tool_call_id.as_str())
        .collect();
    assert!(!ids.contains(&"t2"), "a cut-off call was offered: {ids:?}");
}

/// covers: TOOL-05
#[test]
fn unparseable_arguments_are_not_a_protocol_violation() {
    let r = run(&with_message(&call("t1", "f", &["{not json"])));
    r.assert_ok();
    assert_eq!(
        r.message("m1")["toolCalls"][0]["function"]["arguments"],
        "{not json"
    );
}

/// covers: TOOL-06, TOOL-43
#[test]
fn a_reopened_call_appends_its_arguments() {
    let mut calls = call("t1", "f", &["{\"a\":"]);
    calls.extend(call("t1", "f", &["1}"]));
    let r = run(&with_message(&calls));
    r.assert_ok();
    let tool_calls = r.message("m1")["toolCalls"].as_array().unwrap().clone();
    assert_eq!(tool_calls.len(), 1);
    assert_eq!(tool_calls[0]["function"]["arguments"], "{\"a\":1}");
}

/// covers: TOOL-08
#[test]
fn a_disagreeing_reopen_is_not_detected_as_a_violation() {
    let mut calls = call("t1", "search", &["{}"]);
    calls.extend(call("t1", "lookup", &[]));
    run(&with_message(&calls)).assert_ok();
}

/// covers: TOOL-09
#[test]
fn tool_chunks_expand_and_need_id_and_name_first() {
    let r = run(&with_message(&[
        json!({"type": "TOOL_CALL_CHUNK", "toolCallId": "t1", "toolCallName": "search", "parentMessageId": "m1", "delta": "{\"q\":"}),
        json!({"type": "TOOL_CALL_CHUNK", "toolCallId": "t1", "toolCallName": "search", "delta": "\"agui\"}"}),
    ]));
    r.assert_ok();
    assert!(r.diagnostics.is_empty());
    assert_eq!(
        r.message("m1")["toolCalls"][0]["function"]["arguments"],
        "{\"q\":\"agui\"}"
    );
    run(&in_run(&[
        json!({"type": "TOOL_CALL_CHUNK", "toolCallId": "t1", "delta": "{}"}),
    ]))
    .assert_rejected("First TOOL_CALL_CHUNK must have a toolCallName");
}

/// covers: TOOL-10
#[test]
fn a_conflicting_repeat_on_a_tool_chunk_is_fatal() {
    run(&in_run(&[
        json!({"type": "TOOL_CALL_CHUNK", "toolCallId": "t1", "toolCallName": "search", "delta": "{\"q\":"}),
        json!({"type": "TOOL_CALL_CHUNK", "toolCallId": "t1", "toolCallName": "delete", "delta": "1}"}),
    ]))
    .assert_rejected("does not match the open stream's toolCallName");
    let r = run(&in_run(&[
        json!({"type": "TOOL_CALL_CHUNK", "toolCallId": "t1", "toolCallName": "f", "parentMessageId": "a"}),
        json!({"type": "TOOL_CALL_CHUNK", "toolCallId": "t1", "parentMessageId": "b"}),
    ]));
    r.assert_rejected("does not match the open stream's parentMessageId");
    assert_eq!(r.error.as_ref().map(|e| e.code()), Some(ErrorCode::Chunk));
}

/// covers: TOOL-10
#[test]
fn a_conflicting_tool_chunk_name_is_not_echoed() {
    let r = run(&in_run(&[
        json!({"type": "TOOL_CALL_CHUNK", "toolCallId": "t1", "toolCallName": "search", "delta": "{"}),
        json!({"type": "TOOL_CALL_CHUNK", "toolCallId": "t1", "toolCallName": "delete\u{1b}[31mEVIL", "delta": "}"}),
    ]));
    let e = r.error.as_ref().expect("a conflicting name is fatal");
    assert_eq!(e.code(), ErrorCode::Chunk);
    let shown = e.to_string();
    assert!(
        !shown.contains("delete") && !shown.contains('\u{1b}'),
        "{shown}"
    );
    assert!(!format!("{e:?}").contains("delete"));
}

/// covers: TOOL-11, TOOL-12
#[test]
fn a_result_is_its_own_tool_message_and_does_not_reopen_the_call() {
    let mut body = call("t1", "f", &["{}"]);
    body.push(json!({"type": "TOOL_CALL_RESULT", "messageId": "res1", "toolCallId": "t1", "content": "3 results found."}));
    let r = run(&with_message(&body));
    r.assert_ok();
    let result = r.message("res1");
    assert_eq!(result["role"], "tool");
    assert_eq!(result["toolCallId"], "t1");
    assert_eq!(result["content"], "3 results found.");

    let mut body = call("t1", "f", &["{}"]);
    body.push(json!({"type": "TOOL_CALL_RESULT", "messageId": "res1", "toolCallId": "t1", "content": "ok"}));
    body.push(json!({"type": "TOOL_CALL_ARGS", "toolCallId": "t1", "delta": "more"}));
    run(&with_message(&body)).assert_rejected("No active tool call found with ID 't1'");

    // A client-executed call's result never arrives in the stream.
    run(&with_message(&call("t2", "confirm", &["{}"]))).assert_ok();
}

/// BASIC-15: the parent a call names is a message id the
/// producer used (the reducer mints the carrier), so a result may not reuse it.
#[test]
fn a_result_may_not_reuse_the_minted_parent_id() {
    let body = [
        json!({"type": "TOOL_CALL_START", "toolCallId": "tc1", "toolCallName": "f", "parentMessageId": "m1"}),
        json!({"type": "TOOL_CALL_END", "toolCallId": "tc1"}),
        json!({"type": "TOOL_CALL_RESULT", "messageId": "m1", "toolCallId": "tc1", "content": "ok"}),
    ];
    let r = run(&in_run(&body));
    r.assert_rejected("already names a message in this thread");
    assert_eq!(
        r.error.as_ref().map(|e| e.code()),
        Some(ErrorCode::Identity)
    );

    // The named parent can still gain text afterwards.
    let ok = [
        json!({"type": "TOOL_CALL_START", "toolCallId": "tc1", "toolCallName": "f", "parentMessageId": "m1"}),
        json!({"type": "TOOL_CALL_END", "toolCallId": "tc1"}),
        json!({"type": "TEXT_MESSAGE_START", "messageId": "m1"}),
        json!({"type": "TEXT_MESSAGE_END", "messageId": "m1"}),
    ];
    run(&in_run(&ok)).assert_ok();
}

/// A result sharing a parentless call's id keeps the call.
#[test]
fn a_result_with_the_call_id_keeps_the_calls_carrier() {
    let body = [
        json!({"type": "TOOL_CALL_START", "toolCallId": "x", "toolCallName": "f"}),
        json!({"type": "TOOL_CALL_END", "toolCallId": "x"}),
        json!({"type": "TOOL_CALL_RESULT", "messageId": "x", "toolCallId": "x", "content": "ok"}),
    ];
    let r = run(&in_run(&body));
    r.assert_ok();
    assert_eq!(r.messages.len(), 2, "{:?}", r.messages);
    assert_eq!(r.messages[0]["role"], "assistant");
    assert_eq!(r.messages[0]["toolCalls"][0]["id"], "x");
    assert_eq!(r.messages[1]["role"], "tool");
}

/// covers: TOOL-14, TOOL-16
#[test]
fn the_minted_tool_message_keeps_the_result_shape() {
    let content = json!([
        {"type": "text", "text": "Invoice attached."},
        {"type": "document", "source": {"type": "url", "value": "https://example.com/a.pdf", "mimeType": "application/pdf"}, "metadata": {"title": "A"}},
        {"type": "document", "source": {"type": "file", "value": "file-123", "provider": "openai"}}
    ]);
    let mut body = call("t1", "f", &["{}"]);
    body.push(json!({"type": "TOOL_CALL_RESULT", "messageId": "res1", "toolCallId": "t1", "content": content}));
    let r = run(&with_message(&body));
    r.assert_ok();
    assert_eq!(r.message("res1")["content"], content);
    assert!(r.diagnostics.is_empty());
}

/// covers: TOOL-19
#[test]
fn a_string_only_rendering_concatenates_text_parts() {
    let content: Content = serde_json::from_value(json!([
        {"type": "text", "text": "a"},
        {"type": "image", "source": {"type": "url", "value": "https://x/y.png"}},
        {"type": "text", "text": "b"}
    ]))
    .unwrap();
    assert_eq!(content.flatten_text(), "ab");
}

fn downgraded(content: Value) -> (Value, usize) {
    let sink = Arc::new(CollectingSink::new());
    let mut reporter = Reporter::new(sink.clone(), 256);
    let mut input = json!({
        "threadId": "t", "runId": "r", "state": {}, "tools": [], "context": [],
        "forwardedProps": {}, "protocolVersion": "1.0",
        "messages": [{"id": "tm", "role": "tool", "toolCallId": "t1", "content": content}]
    });
    EraShims::new(LegacyEra::V0_0_39).downgrade_input(&mut input, &mut reporter);
    reporter.end_event(None);
    (input["messages"][0]["content"].clone(), sink.take().len())
}

/// covers: TOOL-20, TOOL-21, TOOL-22
#[test]
fn flattening_for_a_pre_parts_peer_is_announced_as_lossy() {
    let (flat, warnings) = downgraded(json!([
        {"type": "text", "text": "a"},
        {"type": "image", "source": {"type": "url", "value": "https://x/y.png"}}
    ]));
    assert_eq!(flat, "a");
    assert!(warnings >= 1, "dropping a part must warn");

    let (flat, warnings) = downgraded(json!([{"type": "text", "text": "only text"}]));
    assert_eq!(flat, "only text");
    assert_eq!(warnings, 0);
}

/// covers: TOOL-23
#[test]
fn no_placeholder_stands_in_for_a_dropped_part() {
    let (flat, _) = downgraded(json!([
        {"type": "text", "text": "before "},
        {"type": "audio", "source": {"type": "data", "value": "AAAA", "mimeType": "audio/wav"}},
        {"type": "text", "text": "after"}
    ]));
    assert_eq!(flat, "before after");
}

/// covers: TOOL-28
#[test]
fn several_frontend_calls_may_be_left_unanswered() {
    let mut calls = call("t1", "confirm", &["{}"]);
    calls.extend(call("t2", "pick", &["{}"]));
    let r = run(&with_message(&calls));
    r.assert_ok();
    assert_eq!(r.message("m1")["toolCalls"].as_array().unwrap().len(), 2);
}

/// covers: TOOL-37
#[test]
fn call_sequence_violations_are_fatal() {
    let r = run(&in_run(&[
        json!({"type": "TOOL_CALL_ARGS", "toolCallId": "t1", "delta": "{}"}),
    ]));
    r.assert_rejected("No active tool call found with ID 't1'");
    assert_eq!(
        r.error.as_ref().map(|e| e.code()),
        Some(ErrorCode::ToolCall)
    );
    run(&in_run(&[
        json!({"type": "TOOL_CALL_END", "toolCallId": "t1"}),
    ]))
    .assert_rejected("No active tool call found with ID 't1'");
    run(&in_run(&[
        json!({"type": "TOOL_CALL_START", "toolCallId": "t1", "toolCallName": "f"}),
        json!({"type": "TOOL_CALL_START", "toolCallId": "t1", "toolCallName": "f"}),
    ]))
    .assert_rejected("A tool call with ID 't1' is already in progress");
    run(&in_run(&[
        json!({"type": "TOOL_CALL_START", "toolCallId": "t1", "toolCallName": "f"}),
    ]))
    .assert_rejected("Cannot send 'RUN_FINISHED' while tool calls are still active: 't1'");
}

/// covers: TOOL-11
#[test]
fn a_result_under_the_carrier_id_keeps_the_call() {
    let r = run(&in_run(&[
        json!({"type": "TOOL_CALL_START", "toolCallId": "call-1", "toolCallName": "f"}),
        json!({"type": "TOOL_CALL_ARGS", "toolCallId": "call-1", "delta": "{}"}),
        json!({"type": "TOOL_CALL_END", "toolCallId": "call-1"}),
        json!({"type": "TOOL_CALL_RESULT", "messageId": "call-1", "toolCallId": "call-1", "content": "ok"}),
    ]));
    r.assert_ok();
    assert_eq!(r.messages.len(), 2, "{:?}", r.messages);
    assert_eq!(r.messages[0]["role"], "assistant");
    assert_eq!(r.messages[0]["toolCalls"][0]["function"]["arguments"], "{}");
    assert_eq!(r.messages[1]["role"], "tool");
}

/// covers: TOOL-11
#[test]
fn a_call_id_equal_to_a_held_tool_message_mints_its_own_carrier() {
    let r = run(&in_run(&[
        json!({"type": "TOOL_CALL_RESULT", "messageId": "x", "toolCallId": "t0", "content": "ok"}),
        json!({"type": "TOOL_CALL_START", "toolCallId": "x", "toolCallName": "f"}),
        json!({"type": "TOOL_CALL_ARGS", "toolCallId": "x", "delta": "{}"}),
        json!({"type": "TOOL_CALL_END", "toolCallId": "x"}),
    ]));
    r.assert_ok();
    assert_eq!(r.messages.len(), 2, "{:?}", r.messages);
    assert_eq!(r.messages[0]["role"], "tool");
    assert_eq!(r.messages[0]["content"], "ok");
    assert_eq!(r.messages[1]["role"], "assistant");
    assert_eq!(r.messages[1]["toolCalls"][0]["function"]["arguments"], "{}");
}
