//! Lane-table unit tests. Rule tests live in `tests/streaming_chunks.rs`.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use serde_json::json;

use super::*;

fn ev(value: serde_json::Value) -> Event {
    serde_json::from_value(value).expect("test event")
}

fn types(events: &[Event]) -> Vec<&'static str> {
    events.iter().map(|e| e.event_type().as_str()).collect()
}

#[test]
fn closing_a_lane_clears_its_index_entry() {
    let mut x = ChunkExpander::new();
    let mut out = Vec::new();
    x.push(
        ev(json!({"type": "TEXT_MESSAGE_CHUNK", "messageId": "m1", "delta": "a"})),
        &mut out,
    )
    .unwrap();
    assert_eq!(x.by_item[Kind::Text.index()].len(), 1);
    x.push(
        ev(json!({"type": "CUSTOM", "name": "n", "value": 1})),
        &mut out,
    )
    .unwrap();
    assert!(x.is_idle());
    assert!(x.by_item.iter().all(HashMap::is_empty));
}

#[test]
fn close_all_ends_lanes_in_opening_order() {
    let mut x = ChunkExpander::new();
    let mut out = Vec::new();
    for (id, tag) in [("m3", Some("s2")), ("m1", None), ("m2", Some("s1"))] {
        let mut chunk = json!({"type": "TEXT_MESSAGE_CHUNK", "messageId": id, "delta": "x"});
        if let Some(tag) = tag {
            chunk["subagentRunId"] = json!(tag);
        }
        x.push(ev(chunk), &mut out).unwrap();
    }
    out.clear();
    x.close_all(&mut out);
    let ids: Vec<&str> = out
        .iter()
        .map(|e| match e {
            Event::TextMessageEnd(end) => end.message_id.as_str(),
            other => panic!("unexpected {other:?}"),
        })
        .collect();
    assert_eq!(ids, ["m3", "m1", "m2"]);
    assert!(x.is_idle());
    out.clear();
    x.close_all(&mut out);
    assert!(out.is_empty());
}

#[test]
fn one_pending_stream_per_lane_of_any_kind() {
    let mut x = ChunkExpander::new();
    let mut out = Vec::new();
    x.push(
        ev(json!({"type": "TEXT_MESSAGE_CHUNK", "messageId": "m1", "delta": "a"})),
        &mut out,
    )
    .unwrap();
    out.clear();
    x.push(
        ev(json!({"type": "TOOL_CALL_CHUNK", "toolCallId": "t1", "toolCallName": "f"})),
        &mut out,
    )
    .unwrap();
    assert_eq!(types(&out), ["TEXT_MESSAGE_END", "TOOL_CALL_START"]);
    assert!(x.by_item[Kind::Text.index()].is_empty());
    assert_eq!(x.by_item[Kind::Tool.index()].len(), 1);
}

#[test]
fn chunk_errors_quote_a_bounded_preview_of_peer_ids() {
    let big = "x".repeat(100_000);
    let cases = [
        (
            json!({"type": "TOOL_CALL_CHUNK", "toolCallId": big, "toolCallName": "a"}),
            json!({"type": "TOOL_CALL_CHUNK", "toolCallId": big, "toolCallName": "b"}),
            "does not match the open stream's toolCallName",
        ),
        (
            json!({"type": "TOOL_CALL_CHUNK", "toolCallId": "t", "toolCallName": "a",
                   "parentMessageId": "p"}),
            json!({"type": "TOOL_CALL_CHUNK", "toolCallId": "t", "parentMessageId": big}),
            "does not match the open stream's parentMessageId",
        ),
        (
            json!({"type": "TEXT_MESSAGE_CHUNK", "messageId": big, "role": "assistant",
                   "delta": "a"}),
            json!({"type": "TEXT_MESSAGE_CHUNK", "messageId": big, "role": "user"}),
            "does not match the open stream's role",
        ),
        (
            json!({"type": "TEXT_MESSAGE_CHUNK", "messageId": "m", "delta": "a",
                   "subagentRunId": "s1"}),
            json!({"type": "TEXT_MESSAGE_CHUNK", "messageId": "m", "subagentRunId": big}),
            "does not match the owner of the open stream",
        ),
        (
            json!({"type": "TOOL_CALL_CHUNK", "toolCallId": big}),
            json!({"type": "TOOL_CALL_CHUNK", "toolCallId": big}),
            "must have a toolCallName",
        ),
    ];
    for (i, (first, second, wording)) in cases.into_iter().enumerate() {
        let mut x = ChunkExpander::new();
        let mut out = Vec::new();
        let first = x.push(ev(first), &mut out);
        let err = match first {
            Err(e) => e,
            Ok(()) => x.push(ev(second), &mut out).expect_err("mismatch is fatal"),
        };
        let message = err.message();
        assert!(message.len() < 512, "case {i}: {} bytes", message.len());
        assert!(message.contains('\u{2026}'), "case {i}: {message}");
        assert!(message.contains(wording), "case {i}: {message}");
    }
}
