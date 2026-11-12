//! Reducer behaviour whose rule rows live in other packages' files:
//! reopens, tool-call placement, results and pending calls.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;

use serde_json::{json, Value};

use super::*;
use crate::diagnostics::CollectingSink;

struct H {
    r: Reducer,
    sink: Arc<CollectingSink>,
    rep: Reporter,
}

impl H {
    fn new() -> Self {
        Self::from(Reducer::new())
    }

    fn from(r: Reducer) -> Self {
        let sink = Arc::new(CollectingSink::new());
        let rep = Reporter::new(sink.clone(), 64);
        H { r, sink, rep }
    }

    fn push(&mut self, v: Value) -> Changes {
        let e: Event = serde_json::from_value(v).unwrap();
        self.r.apply(e, &mut self.rep).changes
    }

    fn history(&self) -> Value {
        serde_json::to_value(self.r.messages()).unwrap()
    }

    fn codes(&self) -> Vec<&'static str> {
        self.sink.take().iter().map(|d| d.code.as_str()).collect()
    }
}

fn id(s: &str) -> MessageId {
    serde_json::from_value(json!(s)).unwrap()
}

#[test]
fn text_deltas_concatenate_and_reopen_appends() {
    let mut h = H::new();
    h.push(json!({"type": "TEXT_MESSAGE_START", "messageId": "m", "name": "bot"}));
    h.push(json!({"type": "TEXT_MESSAGE_CONTENT", "messageId": "m", "delta": "Hel"}));
    h.push(json!({"type": "TEXT_MESSAGE_CONTENT", "messageId": "m", "delta": "lo"}));
    h.push(json!({"type": "TEXT_MESSAGE_END", "messageId": "m"}));
    h.push(json!({"type": "TEXT_MESSAGE_START", "messageId": "m", "name": "bot"}));
    let c = h.push(json!({"type": "TEXT_MESSAGE_CONTENT", "messageId": "m", "delta": "!"}));
    assert_eq!(c.message, Some(id("m")));
    assert_eq!(
        h.history(),
        json!([{"id": "m", "role": "assistant", "content": "Hello!", "name": "bot"}])
    );
    assert!(h.codes().is_empty());
    assert!(h.r.message(&id("m")).is_some());
}

#[test]
fn disagreeing_text_reopen_keeps_established_values_and_warns() {
    let mut h = H::new();
    h.push(json!({"type": "TEXT_MESSAGE_START", "messageId": "m", "role": "user", "name": "ann"}));
    h.push(json!({"type": "TEXT_MESSAGE_END", "messageId": "m"}));
    h.push(
        json!({"type": "TEXT_MESSAGE_START", "messageId": "m", "role": "assistant", "name": "bob",
        "subagentRunId": "s"}),
    );
    h.push(json!({"type": "TEXT_MESSAGE_CONTENT", "messageId": "m", "delta": "x"}));
    assert_eq!(
        h.history(),
        json!([{"id": "m", "role": "user", "content": "x", "name": "ann"}])
    );
    assert_eq!(h.codes(), ["stream.conflicting_reopen"]);
}

#[test]
fn every_role_builds_its_message_kind() {
    let mut h = H::new();
    for (m, role) in [
        ("d", "developer"),
        ("s", "system"),
        ("u", "user"),
        ("a", "assistant"),
    ] {
        h.push(json!({"type": "TEXT_MESSAGE_START", "messageId": m, "role": role}));
        h.push(json!({"type": "TEXT_MESSAGE_CONTENT", "messageId": m, "delta": m}));
    }
    assert_eq!(
        h.history(),
        json!([
            {"id": "d", "role": "developer", "content": "d"},
            {"id": "s", "role": "system", "content": "s"},
            {"id": "u", "role": "user", "content": "u"},
            {"id": "a", "role": "assistant", "content": "a"}
        ])
    );
}

#[test]
fn orphan_fragments_invent_nothing() {
    let mut h = H::new();
    assert_eq!(
        h.push(json!({"type": "TEXT_MESSAGE_CONTENT", "messageId": "x", "delta": "a"}))
            .message,
        None
    );
    h.push(json!({"type": "TOOL_CALL_ARGS", "toolCallId": "c", "delta": "{}"}));
    h.push(json!({"type": "TOOL_CALL_END", "toolCallId": "c"}));
    h.push(json!({"type": "REASONING_MESSAGE_CONTENT", "messageId": "r", "delta": "a"}));
    h.push(json!({"type": "TOOL_CALL_CHUNK", "toolCallId": "c", "delta": "{"}));
    assert_eq!(h.history(), json!([]));
    // Each discarded fragment is reported, once (the nameless chunk too,
    // as TOOL_CALL_ARGS is; the pipeline's verifier ends the stream at it
    // first, TOOL-09).
    assert_eq!(h.codes(), ["stream.missing_target"; 5]);
}

#[test]
fn appends_into_part_lists() {
    let mut h = H::from(Reducer::with_history(
        serde_json::from_value(json!([{"id": "u", "role": "user", "content": [
            {"type": "text", "text": "a"}]}]))
        .unwrap(),
        json!({}),
    ));
    h.push(json!({"type": "TEXT_MESSAGE_START", "messageId": "u", "role": "user"}));
    h.push(json!({"type": "TEXT_MESSAGE_CONTENT", "messageId": "u", "delta": "b"}));
    assert_eq!(
        h.history()[0]["content"],
        json!([{"type": "text", "text": "ab"}])
    );
}

#[test]
fn tool_calls_attach_to_the_named_parent_or_mint_one() {
    let mut h = H::new();
    h.push(json!({"type": "TEXT_MESSAGE_START", "messageId": "a1"}));
    h.push(json!({"type": "TEXT_MESSAGE_END", "messageId": "a1"}));
    h.push(json!({"type": "TOOL_CALL_START", "toolCallId": "c1", "toolCallName": "f", "parentMessageId": "a1"}));
    // Unknown parent: minted, tool-call only.
    h.push(json!({"type": "TOOL_CALL_START", "toolCallId": "c2", "toolCallName": "g", "parentMessageId": "a2"}));
    h.push(json!({"type": "TOOL_CALL_ARGS", "toolCallId": "c2", "delta": "{\"x\":"}));
    h.push(json!({"type": "TOOL_CALL_ARGS", "toolCallId": "c2", "delta": "1}"}));
    // No parent: joins the latest assistant message of this run.
    h.push(json!({"type": "TOOL_CALL_START", "toolCallId": "c3", "toolCallName": "h"}));
    let history = h.history();
    assert_eq!(history[0]["toolCalls"][0]["id"], "c1");
    assert_eq!(
        history[1],
        json!({"id": "a2", "role": "assistant", "toolCalls": [
            {"id": "c2", "type": "function", "function": {"name": "g", "arguments": "{\"x\":1}"}},
            {"id": "c3", "type": "function", "function": {"name": "h", "arguments": ""}}
        ]})
    );
}

#[test]
fn parentless_call_after_a_non_assistant_mints_a_carrier() {
    let mut h = H::new();
    h.push(json!({"type": "TEXT_MESSAGE_START", "messageId": "u", "role": "user"}));
    let c = h.push(json!({"type": "TOOL_CALL_START", "toolCallId": "c1", "toolCallName": "f"}));
    assert_eq!(c.message, Some(id("c1")));
    assert_eq!(h.history()[1]["id"], "c1");
    // A parent naming a non-assistant message also mints under the call id.
    h.push(json!({"type": "TOOL_CALL_START", "toolCallId": "c2", "toolCallName": "f", "parentMessageId": "u"}));
    assert_eq!(h.history()[2]["id"], "c2");
}

#[test]
fn text_after_a_tool_only_carrier_fills_it() {
    let mut h = H::new();
    h.push(json!({"type": "TOOL_CALL_START", "toolCallId": "c1", "toolCallName": "f", "parentMessageId": "a"}));
    h.push(json!({"type": "TEXT_MESSAGE_START", "messageId": "a", "name": "n"}));
    h.push(json!({"type": "TEXT_MESSAGE_CONTENT", "messageId": "a", "delta": "t"}));
    assert_eq!(h.history()[0]["content"], "t");
    assert_eq!(h.history()[0]["name"], "n");
    assert!(h.codes().is_empty());
}

#[test]
fn tool_reopen_with_new_parent_keeps_placement() {
    let mut h = H::new();
    h.push(json!({"type": "TOOL_CALL_START", "toolCallId": "c", "toolCallName": "f", "parentMessageId": "a1"}));
    h.push(json!({"type": "TOOL_CALL_ARGS", "toolCallId": "c", "delta": "{"}));
    h.push(json!({"type": "TOOL_CALL_END", "toolCallId": "c"}));
    h.push(json!({"type": "TOOL_CALL_START", "toolCallId": "c", "toolCallName": "f", "parentMessageId": "a1"}));
    assert!(h.codes().is_empty(), "an agreeing reopen is silent");
    h.push(json!({"type": "TOOL_CALL_START", "toolCallId": "d", "toolCallName": "k", "parentMessageId": "a1"}));
    h.push(json!({"type": "TOOL_CALL_START", "toolCallId": "c", "toolCallName": "g", "parentMessageId": "a2"}));
    h.push(json!({"type": "TOOL_CALL_ARGS", "toolCallId": "c", "delta": "}"}));
    h.push(json!({"type": "TOOL_CALL_ARGS", "toolCallId": "d", "delta": "x"}));
    let diagnostics = h.sink.take();
    assert_eq!(
        diagnostics
            .iter()
            .map(|d| d.code.as_str())
            .collect::<Vec<_>>(),
        ["stream.conflicting_reopen"]
    );
    assert!(
        diagnostics[0].message.contains("established parent"),
        "{}",
        diagnostics[0].message
    );
    let history = h.history();
    assert_eq!(history.as_array().unwrap().len(), 1, "no message a2 minted");
    assert_eq!(
        history[0]["toolCalls"],
        json!([
            {"id": "c", "type": "function", "function": {"name": "g", "arguments": "{}"}},
            {"id": "d", "type": "function", "function": {"name": "k", "arguments": "x"}}
        ])
    );
}

#[test]
fn identical_reopen_under_non_assistant_parent_is_silent() {
    let mut h = H::new();
    h.push(json!({"type": "TEXT_MESSAGE_START", "messageId": "u", "role": "user"}));
    h.push(json!({"type": "TEXT_MESSAGE_END", "messageId": "u"}));
    h.push(json!({"type": "TOOL_CALL_START", "toolCallId": "tc1", "toolCallName": "f", "parentMessageId": "u"}));
    h.push(json!({"type": "TOOL_CALL_END", "toolCallId": "tc1"}));
    h.push(json!({"type": "TOOL_CALL_START", "toolCallId": "tc2", "toolCallName": "f", "parentMessageId": "tc1"}));
    h.push(json!({"type": "TOOL_CALL_END", "toolCallId": "tc2"}));
    h.push(json!({"type": "TOOL_CALL_START", "toolCallId": "tc1", "toolCallName": "f", "parentMessageId": "u"}));
    assert!(h.codes().is_empty(), "an identical reopen is silent");
    let ids: Vec<_> = h.history()[1]["toolCalls"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["id"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(ids, ["tc1", "tc2"]);
}

#[test]
fn duplicate_call_ids_across_messages_stay_consistent() {
    let mut h = H::new();
    h.push(json!({"type": "MESSAGES_SNAPSHOT", "messages": [
        {"id": "A", "role": "assistant", "toolCalls": [
            {"id": "x", "type": "function", "function": {"name": "f", "arguments": ""}},
            {"id": "c1", "type": "function", "function": {"name": "f", "arguments": ""}}]},
        {"id": "B", "role": "assistant", "toolCalls": [
            {"id": "c1", "type": "function", "function": {"name": "f", "arguments": ""}}]}
    ]}));
    h.push(json!({"type": "TOOL_CALL_START", "toolCallId": "x", "toolCallName": "f", "parentMessageId": "B"}));
    h.push(json!({"type": "TOOL_CALL_ARGS", "toolCallId": "c1", "delta": "ZZ"}));
    let history = h.history();
    let args = |m: usize, c: usize| history[m]["toolCalls"][c]["function"]["arguments"].clone();
    // The first occurrence of a duplicated id owns routing.
    assert_eq!(args(0, 1), "ZZ");
    assert_eq!(args(1, 0), "");
}

#[test]
fn results_are_tool_messages_and_pending_calls_are_derived() {
    let mut h = H::new();
    h.push(json!({"type": "RUN_STARTED", "threadId": "t", "runId": "r"}));
    for c in ["c1", "c2", "c3"] {
        h.push(json!({"type": "TOOL_CALL_START", "toolCallId": c, "toolCallName": "f", "parentMessageId": "a"}));
        h.push(json!({"type": "TOOL_CALL_ARGS", "toolCallId": c, "delta": c}));
        h.push(json!({"type": "TOOL_CALL_END", "toolCallId": c}));
    }
    let c = h.push(
        json!({"type": "TOOL_CALL_RESULT", "messageId": "t2", "toolCallId": "c2",
        "content": [{"type": "text", "text": "ok"}]}),
    );
    assert_eq!(c.message, Some(id("t2")));
    // `pendingToolCallIds` is not trusted: the stream decides (TOOL-32).
    h.push(
        json!({"type": "RUN_FINISHED", "threadId": "t", "runId": "r",
        "outcome": {"type": "success", "pendingToolCallIds": []},
        "usage": [{"inputTokens": 3}]}),
    );
    let pending: Vec<_> =
        h.r.pending_tool_calls()
            .iter()
            .map(|p| (p.tool_call_id.clone(), p.arguments.clone()))
            .collect();
    assert_eq!(
        pending,
        [
            (
                serde_json::from_value(json!("c1")).unwrap(),
                "c1".to_owned()
            ),
            (
                serde_json::from_value(json!("c3")).unwrap(),
                "c3".to_owned()
            )
        ]
    );
    assert_eq!(h.r.pending_tool_calls()[0].message_id, id("a"));
    assert_eq!(h.r.usage().len(), 1);
    assert_eq!(
        h.history()[1],
        json!({"id": "t2", "role": "tool", "toolCallId": "c2",
        "content": [{"type": "text", "text": "ok"}]})
    );
    assert_eq!(h.r.run_message_ids(), [id("a"), id("t2")]);
    // A new run clears the previous run's pending calls and usage.
    h.push(json!({"type": "RUN_STARTED", "threadId": "t", "runId": "r2"}));
    assert!(h.r.pending_tool_calls().is_empty() && h.r.usage().is_empty());
    assert!(h.r.run_message_ids().is_empty());
}

#[test]
fn duplicate_result_id_replaces_in_place() {
    let mut h = H::new();
    h.push(
        json!({"type": "TOOL_CALL_RESULT", "messageId": "t", "toolCallId": "c", "content": "a"}),
    );
    h.push(json!({"type": "TEXT_MESSAGE_START", "messageId": "m"}));
    h.push(
        json!({"type": "TOOL_CALL_RESULT", "messageId": "t", "toolCallId": "c", "content": "b"}),
    );
    assert_eq!(h.history()[0]["content"], "b");
    assert_eq!(h.history().as_array().unwrap().len(), 2);
}

/// pipeline-verify-3: per-run call sets inflated by one run are dropped, not
/// cleared at O(capacity), once later runs are small.
#[test]
fn oversized_run_call_sets_are_dropped_after_a_small_run() {
    let floor = crate::pipeline::RETAIN_FLOOR;
    let mut h = H::new();
    h.push(json!({"type": "RUN_STARTED", "threadId": "t", "runId": "r1"}));
    for i in 0..100_000 {
        let c = format!("c{i}");
        h.push(json!({"type": "TOOL_CALL_START", "toolCallId": c, "toolCallName": "f"}));
        h.push(json!({"type": "TOOL_CALL_END", "toolCallId": c}));
        h.push(
            json!({"type": "TOOL_CALL_RESULT", "messageId": format!("r{i}"),
            "toolCallId": c, "content": "ok"}),
        );
    }
    h.push(json!({"type": "RUN_FINISHED", "threadId": "t", "runId": "r1"}));
    h.push(json!({"type": "RUN_STARTED", "threadId": "t", "runId": "r2"}));
    h.push(json!({"type": "TOOL_CALL_START", "toolCallId": "z", "toolCallName": "f"}));
    h.push(json!({"type": "TOOL_CALL_END", "toolCallId": "z"}));
    h.push(
        json!({"type": "TOOL_CALL_RESULT", "messageId": "rz", "toolCallId": "z", "content": "ok"}),
    );
    h.push(json!({"type": "RUN_FINISHED", "threadId": "t", "runId": "r2"}));
    h.push(json!({"type": "RUN_STARTED", "threadId": "t", "runId": "r3"}));
    for set in [
        &h.r.run_calls.started_set,
        &h.r.run_calls.ended,
        &h.r.run_calls.answered,
    ] {
        assert!(set.capacity() <= floor, "capacity {}", set.capacity());
    }
}

/// A result whose message id equals a parentless call's id (the id its
/// carrier was minted under) is the producer's own message: the carrier
/// gives the id up (it is renamed) and keeps the call.
#[test]
fn result_with_the_carrier_id_renames_the_carrier_and_keeps_the_call() {
    let mut h = H::new();
    h.push(json!({"type": "RUN_STARTED", "threadId": "t", "runId": "r"}));
    h.push(json!({"type": "TOOL_CALL_START", "toolCallId": "x", "toolCallName": "f"}));
    h.push(json!({"type": "TOOL_CALL_END", "toolCallId": "x"}));
    h.push(
        json!({"type": "TOOL_CALL_RESULT", "messageId": "x", "toolCallId": "x", "content": "ok"}),
    );
    h.push(json!({"type": "RUN_FINISHED", "threadId": "t", "runId": "r"}));
    let history = h.history();
    assert_eq!(history.as_array().unwrap().len(), 2, "{history}");
    assert_eq!(history[0]["role"], "assistant");
    assert_eq!(history[0]["id"], "x~1");
    assert_eq!(history[0]["toolCalls"][0]["id"], "x");
    assert_eq!(history[1]["role"], "tool");
    assert_eq!(history[1]["id"], "x");
    assert!(h.r.pending_tool_calls().is_empty());
    // The id names the result, the carrier its new id, each once.
    assert!(matches!(h.r.message(&id("x")), Some(Message::Tool(_))));
    assert!(matches!(
        h.r.message(&id("x~1")),
        Some(Message::Assistant(_))
    ));
    assert_eq!(h.r.run_message_ids(), &[id("x~1"), id("x")]);
    h.push(json!({"type": "REASONING_ENCRYPTED_VALUE", "subtype": "tool-call", "entityId": "x", "encryptedValue": "v"}));
    assert_eq!(h.history()[0]["toolCalls"][0]["encryptedValue"], "v");
    assert!(h.codes().is_empty());
}

/// A carrier the reducer minted under a parentless call's id does not
/// capture a later message the producer opens under that id: reasoning or
/// text opened with it is a new message, not a reopen of the carrier.
#[test]
fn a_message_opened_under_a_carrier_id_is_a_new_message() {
    let stream = |start: Value, delta: &str| {
        let mut h = H::new();
        h.push(json!({"type": "RUN_STARTED", "threadId": "t", "runId": "r"}));
        h.push(json!({"type": "TOOL_CALL_START", "toolCallId": "c", "toolCallName": "f"}));
        h.push(json!({"type": "TOOL_CALL_END", "toolCallId": "c"}));
        let kind = start["type"].as_str().unwrap().replace("_START", "");
        h.push(start);
        let c =
            h.push(json!({"type": format!("{kind}_CONTENT"), "messageId": "c", "delta": delta}));
        assert_eq!(c.message, Some(id("c")));
        h.push(json!({"type": format!("{kind}_END"), "messageId": "c"}));
        h.push(json!({"type": "RUN_FINISHED", "threadId": "t", "runId": "r"}));
        h
    };
    let h = stream(
        json!({"type": "REASONING_MESSAGE_START", "messageId": "c", "role": "reasoning"}),
        "secret thinking",
    );
    assert!(h.codes().is_empty());
    assert_eq!(
        h.history(),
        json!([
            {"id": "c~1", "role": "assistant", "toolCalls": [
                {"id": "c", "type": "function", "function": {"name": "f", "arguments": ""}}
            ]},
            {"id": "c", "role": "reasoning", "content": "secret thinking"}
        ])
    );
    assert!(matches!(h.r.message(&id("c")), Some(Message::Reasoning(_))));
    assert_eq!(h.r.run_message_ids(), &[id("c~1"), id("c")]);

    let h = stream(
        json!({"type": "TEXT_MESSAGE_START", "messageId": "c", "role": "user"}),
        "user words",
    );
    assert!(h.codes().is_empty());
    let history = h.history();
    assert_eq!(history[0]["id"], "c~1");
    assert!(history[0].get("content").is_none(), "{history}");
    assert_eq!(
        history[1],
        json!({"id": "c", "role": "user", "content": "user words"})
    );
}

/// A carrier the producer adopted (it named the id as a later call's
/// parent) is the producer's message: a text start under its id fills it.
#[test]
fn an_adopted_carrier_is_reopened_by_its_id() {
    let mut h = H::new();
    h.push(json!({"type": "RUN_STARTED", "threadId": "t", "runId": "r"}));
    h.push(json!({"type": "TOOL_CALL_START", "toolCallId": "c", "toolCallName": "f"}));
    h.push(json!({"type": "TOOL_CALL_END", "toolCallId": "c"}));
    h.push(json!({"type": "TOOL_CALL_START", "toolCallId": "d", "toolCallName": "g", "parentMessageId": "c"}));
    h.push(json!({"type": "TOOL_CALL_END", "toolCallId": "d"}));
    h.push(json!({"type": "TEXT_MESSAGE_START", "messageId": "c"}));
    h.push(json!({"type": "TEXT_MESSAGE_CONTENT", "messageId": "c", "delta": "t"}));
    let history = h.history();
    assert_eq!(history.as_array().unwrap().len(), 1, "{history}");
    assert_eq!(history[0]["id"], "c");
    assert_eq!(history[0]["content"], "t");
    assert_eq!(history[0]["toolCalls"].as_array().unwrap().len(), 2);
    assert!(h.codes().is_empty());
}

/// A carrier minted in an earlier run of the stream gives its id up the same
/// way; only the new message is the later run's.
#[test]
fn a_carrier_from_an_earlier_run_gives_its_id_up() {
    let mut h = H::new();
    h.push(json!({"type": "RUN_STARTED", "threadId": "t", "runId": "r1"}));
    h.push(json!({"type": "TOOL_CALL_START", "toolCallId": "c", "toolCallName": "f"}));
    h.push(json!({"type": "TOOL_CALL_END", "toolCallId": "c"}));
    h.push(json!({"type": "RUN_FINISHED", "threadId": "t", "runId": "r1"}));
    h.push(json!({"type": "RUN_STARTED", "threadId": "t", "runId": "r2"}));
    h.push(
        json!({"type": "ACTIVITY_SNAPSHOT", "messageId": "c", "activityType": "a", "content": {}}),
    );
    let history = h.history();
    assert_eq!(history[0]["id"], "c~1");
    assert_eq!(history[1]["role"], "activity");
    assert_eq!(h.r.run_message_ids(), &[id("c")]);
    assert!(h.codes().is_empty());
}

/// pipeline-verify-1 (amendment A): a restored `[Assistant x, Tool x]` history
/// indexes the earlier message.
#[test]
fn restored_id_clash_keeps_the_index_on_the_earlier_message() {
    let messages: Vec<Message> = serde_json::from_value(json!([
        {"id": "x", "role": "assistant", "toolCalls": [
            {"id": "x", "type": "function", "function": {"name": "f", "arguments": ""}}]},
        {"id": "x", "role": "tool", "toolCallId": "x", "content": "ok"}
    ]))
    .unwrap();
    let r = Reducer::with_history(messages, json!({}));
    assert!(matches!(r.message(&id("x")), Some(Message::Assistant(_))));
}

#[test]
fn a_result_never_replaces_a_held_non_tool_message() {
    let mut h = H::new();
    h.push(json!({"type": "MESSAGES_SNAPSHOT", "messages": [
        {"id": "A", "role": "assistant", "toolCalls": [
            {"id": "c1", "type": "function", "function": {"name": "f", "arguments": ""}}]},
        {"id": "B", "role": "assistant", "toolCalls": [
            {"id": "c1", "type": "function", "function": {"name": "f", "arguments": ""}}]}
    ]}));
    h.push(
        json!({"type": "TOOL_CALL_RESULT", "messageId": "A", "toolCallId": "zz", "content": "r"}),
    );
    h.push(json!({"type": "TOOL_CALL_ARGS", "toolCallId": "c1", "delta": "Z"}));
    let history = h.history();
    assert_eq!(history.as_array().unwrap().len(), 3, "{history}");
    assert_eq!(history[0]["role"], "assistant");
    assert_eq!(history[0]["toolCalls"][0]["function"]["arguments"], "Z");
    assert_eq!(history[2]["role"], "tool");
    // A result replaces only a tool message answering the same call (TS parity).
    h.push(
        json!({"type": "TOOL_CALL_RESULT", "messageId": "t", "toolCallId": "k1", "content": "a"}),
    );
    h.push(
        json!({"type": "TOOL_CALL_RESULT", "messageId": "t", "toolCallId": "k2", "content": "b"}),
    );
    assert_eq!(h.history().as_array().unwrap().len(), 5);
}

#[test]
fn interrupted_run_exposes_interrupts() {
    let mut h = H::new();
    h.push(json!({"type": "RUN_STARTED", "threadId": "t", "runId": "r"}));
    h.push(
        json!({"type": "RUN_FINISHED", "threadId": "t", "runId": "r",
        "outcome": {"type": "interrupt", "interrupts": [{"id": "i1", "reason": "approval"}]}}),
    );
    assert_eq!(h.r.open_interrupts().len(), 1);
    // RUN_ERROR alone (no RUN_STARTED in between) must clear them.
    h.push(json!({"type": "RUN_ERROR", "message": "boom"}));
    assert!(h.r.open_interrupts().is_empty());
}

fn restored_with_interrupt() -> H {
    let interrupts: Vec<Interrupt> =
        serde_json::from_value(json!([{"id": "i1", "reason": "approval"}])).unwrap();
    let r = Reducer::restore(Vec::new(), json!({}), Vec::new(), interrupts);
    assert_eq!(r.open_interrupts().len(), 1);
    H::from(r)
}

#[test]
fn run_finished_success_clears_restored_interrupts() {
    let mut h = restored_with_interrupt();
    h.push(
        json!({"type": "RUN_FINISHED", "threadId": "t", "runId": "r",
        "outcome": {"type": "success"}}),
    );
    assert!(h.r.open_interrupts().is_empty());
}

#[test]
fn run_error_clears_restored_interrupts() {
    let mut h = restored_with_interrupt();
    h.push(json!({"type": "RUN_ERROR", "message": "boom"}));
    assert!(h.r.open_interrupts().is_empty());
}

#[test]
fn defaults_and_history_indices() {
    assert_eq!(Reducer::default().state(), &json!({}));
    let r = Reducer::with_history(
        serde_json::from_value(json!([
            {"id": "a", "role": "assistant", "toolCalls": [
                {"id": "c", "type": "function", "function": {"name": "f", "arguments": ""}}]}
        ]))
        .unwrap(),
        json!(null),
    );
    assert!(r.run_message_ids().is_empty());
    // The history's unanswered call is pending (TOOL-32, TOOL-33).
    assert_eq!(r.pending_tool_calls().len(), 1);
    let mut h = H::from(r);
    h.push(json!({"type": "REASONING_ENCRYPTED_VALUE", "subtype": "tool-call", "entityId": "c", "encryptedValue": "v"}));
    assert_eq!(h.history()[0]["toolCalls"][0]["encryptedValue"], "v");
    assert!(h.codes().is_empty());
}

#[test]
fn snapshot_state_is_shared_then_taken_over_without_a_copy() {
    let mut h = H::new();
    let e: Event =
        serde_json::from_value(json!({"type": "STATE_SNAPSHOT", "snapshot": {"a": [1]}})).unwrap();
    let processed = h.r.apply(e, &mut h.rep);
    let Event::StateSnapshot(s) = &*processed.event else {
        panic!()
    };
    // The reducer reads the state through the shared event: same allocation.
    assert!(std::ptr::eq(h.r.state(), &s.snapshot));
    let held = processed.event;
    // Mutating while the application still holds the update copies.
    h.push(json!({"type": "STATE_DELTA", "delta": [{"op": "add", "path": "/b", "value": 2}]}));
    assert_eq!(h.r.state(), &json!({"a": [1], "b": 2}));
    let Event::StateSnapshot(s) = &*held else {
        panic!()
    };
    assert_eq!(
        s.snapshot,
        json!({"a": [1]}),
        "the application's event is untouched"
    );
    drop(held);
    // Once dropped, the next snapshot is taken over by move.
    let e: Event =
        serde_json::from_value(json!({"type": "STATE_SNAPSHOT", "snapshot": {"big": "x"}}))
            .unwrap();
    let processed = h.r.apply(e, &mut h.rep);
    let Event::StateSnapshot(s) = &*processed.event else {
        panic!()
    };
    let inner = match &s.snapshot {
        Value::Object(m) => &m["big"] as *const Value,
        _ => panic!(),
    };
    drop(processed);
    // A `test` op needs owned access but changes nothing: the value was moved
    // out of the event, so its members did not move.
    h.push(json!({"type": "STATE_DELTA", "delta": [{"op": "test", "path": "/big", "value": "x"}]}));
    let after = match h.r.state() {
        Value::Object(m) => &m["big"] as *const Value,
        _ => panic!(),
    };
    assert_eq!(inner, after, "taken over by move, not copied");
}

#[test]
fn run_message_ids_follow_snapshots() {
    let mut h = H::new();
    h.push(json!({"type": "TEXT_MESSAGE_START", "messageId": "old"}));
    h.push(json!({"type": "RUN_STARTED", "threadId": "t", "runId": "r"}));
    h.push(json!({"type": "TEXT_MESSAGE_START", "messageId": "m1"}));
    h.push(json!({"type": "TEXT_MESSAGE_START", "messageId": "m2"}));
    h.push(json!({"type": "MESSAGES_SNAPSHOT", "messages": [
        {"id": "old", "role": "assistant", "content": ""},
        {"id": "m2", "role": "assistant", "content": ""},
        {"id": "n", "role": "user", "content": "x"}
    ]}));
    assert_eq!(h.r.run_message_ids(), [id("m2"), id("n")]);
}

#[test]
fn restore_keeps_pending_and_interrupts() {
    let pending = vec![PendingToolCall::new(
        serde_json::from_value(json!("c")).unwrap(),
        "f",
        "{}",
        id("a"),
    )];
    let interrupts: Vec<Interrupt> =
        serde_json::from_value(json!([{"id": "i1", "reason": "approval"}])).unwrap();
    let r = Reducer::restore(Vec::new(), json!({"s": 1}), pending.clone(), interrupts);
    assert_eq!(r.pending_tool_calls(), &pending[..]);
    assert_eq!(r.open_interrupts().len(), 1);
    assert_eq!(r.state(), &json!({"s": 1}));
    assert_eq!(
        serde_json::to_value(&pending[0]).unwrap(),
        json!({"toolCallId": "c", "name": "f", "arguments": "{}", "messageId": "a"})
    );
}

#[test]
fn content_deltas_recycle_their_arc_only_when_dropped() {
    let mut h = H::new();
    let ev = |v: Value| -> Event { serde_json::from_value(v).unwrap() };
    h.push(json!({"type": "TEXT_MESSAGE_START", "messageId": "m"}));
    let delta = |d: &str| ev(json!({"type": "TEXT_MESSAGE_CONTENT", "messageId": "m", "delta": d}));

    // Dropped promptly: the next event lands in the same allocation.
    let first = h.r.apply(delta("a"), &mut h.rep).event;
    let at = Arc::as_ptr(&first);
    drop(first);
    let second = h.r.apply(delta("b"), &mut h.rep).event;
    assert_eq!(Arc::as_ptr(&second), at);

    // Still held: a fresh allocation, and the held event is untouched.
    let third = h.r.apply(delta("c"), &mut h.rep).event;
    assert_ne!(Arc::as_ptr(&third), Arc::as_ptr(&second));
    assert!(matches!(&*second, Event::TextMessageContent(e) if e.delta == "b"));
    drop((second, third));

    // A payload event is never retained: the application's handle is unique,
    // even when it reuses a recycled allocation.
    let snap =
        h.r.apply(
            ev(json!({"type": "CUSTOM", "name": "n", "value": [1, 2]})),
            &mut h.rep,
        )
        .event;
    assert_eq!(Arc::strong_count(&snap), 1);
    assert!(Arc::try_unwrap(snap).is_ok());
    assert_eq!(h.history()[0]["content"], json!("abc"));
}

/// The reconcile this module had before it worked in place: rebuild the
/// whole history from the snapshot. Also returns the ids appended and, per
/// held message, whether it was kept.
fn oracle(held: Vec<Message>, snapshot: &[Message]) -> (Vec<Message>, Vec<MessageId>, Vec<bool>) {
    let restates_activity = snapshot.iter().any(|m| matches!(m, Message::Activity(_)));
    let restates_reasoning = snapshot.iter().any(|m| matches!(m, Message::Reasoning(_)));
    let mut placed = vec![false; snapshot.len()];
    let mut first: HashMap<&MessageId, usize> = HashMap::new();
    for (j, m) in snapshot.iter().enumerate() {
        match first.entry(message_id(m)) {
            std::collections::hash_map::Entry::Vacant(v) => {
                v.insert(j);
            }
            std::collections::hash_map::Entry::Occupied(_) => placed[j] = true,
        }
    }
    let mut out = Vec::new();
    let mut kept = Vec::new();
    for m in held {
        let keep = match first.get(message_id(&m)) {
            Some(&j) if placed[j] => None,
            Some(&j) => {
                placed[j] = true;
                Some(snapshot[j].clone())
            }
            None => match m {
                Message::Activity(_) if !restates_activity => Some(m),
                Message::Reasoning(_) if !restates_reasoning => Some(m),
                _ => None,
            },
        };
        kept.push(keep.is_some());
        out.extend(keep);
    }
    let mut appended = Vec::new();
    for (j, m) in snapshot.iter().enumerate() {
        if !placed[j] {
            appended.push(message_id(m).clone());
            out.push(m.clone());
        }
    }
    (out, appended, kept)
}

fn call_json(id: &str) -> Value {
    json!({"id": id, "type": "function", "function": {"name": "f", "arguments": ""}})
}

/// A mixed history built by events, with a carrier whose id clashes with a
/// retained reasoning message.
fn mixed() -> H {
    let mut h = H::new();
    for e in [
        json!({"type": "RUN_STARTED", "threadId": "t", "runId": "r"}),
        json!({"type": "TEXT_MESSAGE_START", "messageId": "u", "role": "user"}),
        json!({"type": "TEXT_MESSAGE_END", "messageId": "u"}),
        json!({"type": "TOOL_CALL_START", "toolCallId": "c1", "toolCallName": "f", "parentMessageId": "a1"}),
        json!({"type": "TOOL_CALL_START", "toolCallId": "c2", "toolCallName": "f", "parentMessageId": "a1"}),
        json!({"type": "ACTIVITY_SNAPSHOT", "messageId": "act", "activityType": "t", "content": {"k": 1}}),
        json!({"type": "REASONING_MESSAGE_START", "messageId": "c9", "role": "reasoning"}),
        json!({"type": "REASONING_MESSAGE_END", "messageId": "c9"}),
        json!({"type": "TOOL_CALL_START", "toolCallId": "c9", "toolCallName": "f"}),
        json!({"type": "ACTIVITY_SNAPSHOT", "messageId": "act2", "activityType": "t", "content": {}}),
        json!({"type": "TEXT_MESSAGE_START", "messageId": "u2", "role": "user"}),
    ] {
        h.push(e);
    }
    h
}

fn user(id: &str) -> Value {
    json!({"id": id, "role": "user", "content": "x"})
}

fn assert_reconciles_like_a_rebuild(mut h: H, snapshots: Vec<Value>) {
    for snap in snapshots {
        let held = h.r.messages().to_vec();
        let parsed: Vec<Message> = serde_json::from_value(snap.clone()).unwrap();
        let base = held.len() - h.r.run_ids.len();
        let (expected, appended, kept) = oracle(held, &parsed);
        // The run's messages are the history's tail: its ids follow them by
        // position, a duplicate id included.
        let mut run_ids: Vec<MessageId> = (h.r.run_ids.iter().zip(&kept[base..]))
            .filter(|(_, &k)| k)
            .map(|(id, _)| id.clone())
            .collect();
        h.push(json!({"type": "MESSAGES_SNAPSHOT", "messages": snap}));
        let fresh = Reducer::restore(expected.clone(), json!({}), Vec::new(), Vec::new());
        assert_eq!(h.r.messages(), &expected[..], "history after {snap}");
        assert_eq!(h.r.index, fresh.index, "index after {snap}");
        assert_eq!(h.r.calls, fresh.calls, "calls after {snap}");
        assert_eq!(h.r.call_dups, fresh.call_dups, "call holders after {snap}");
        assert_eq!(h.r.class_counts, fresh.class_counts, "classes after {snap}");
        assert_eq!(h.r.dup_extra, fresh.dup_extra, "duplicates after {snap}");
        run_ids.extend(appended);
        assert_eq!(h.r.run_message_ids(), &run_ids[..], "run ids after {snap}");
    }
}

#[test]
fn reconcile_in_place_matches_rebuild() {
    let a1_calls = json!({"id": "a1", "role": "assistant", "toolCalls": [call_json("c1")]});
    let sequences: Vec<Vec<Value>> = vec![
        vec![json!([])],
        vec![json!([user("u"), a1_calls.clone(), user("u2")])],
        vec![
            json!([user("u2")]),
            json!([user("z"), user("u2")]),
            json!([]),
        ],
        vec![json!([{"id": "act", "role": "activity", "activityType": "t", "content": {"n": 2}}])],
        vec![json!([{"id": "c9", "role": "reasoning", "content": "r"}, user("u")])],
        // Restated id of a clashing duplicate, then a plain drop.
        vec![json!([user("c9")]), json!([])],
        vec![json!([a1_calls.clone(), user("c1")]), json!([user("c1")])],
        vec![
            json!([user("u"), user("u"), user("n")]),
            json!([user("n"), user("u")]),
        ],
    ];
    for snapshots in sequences {
        assert_reconciles_like_a_rebuild(mixed(), snapshots);
    }

    // A restored history that already holds duplicates.
    let restored = || {
        H::from(Reducer::with_history(
            serde_json::from_value(json!([
                {"id": "x", "role": "assistant", "content": "a"},
                {"id": "act", "role": "activity", "activityType": "t", "content": {}},
                {"id": "x", "role": "reasoning", "content": "r"},
                {"id": "y", "role": "user", "content": "u"},
                {"id": "x", "role": "user", "content": "v"}
            ]))
            .unwrap(),
            json!({}),
        ))
    };
    for snapshots in [
        vec![json!([])],
        vec![json!([user("x")])],
        vec![json!([user("y")]), json!([])],
        vec![json!([{"id": "x", "role": "reasoning", "content": "q"}])],
    ] {
        assert_reconciles_like_a_rebuild(restored(), snapshots);
    }
}

fn assistant_with(id: &str, call: Option<&str>) -> Value {
    match call {
        Some(c) => json!({"id": id, "role": "assistant", "toolCalls": [call_json(c)]}),
        None => json!({"id": id, "role": "assistant", "content": ""}),
    }
}

fn args_of(h: &H, message: &str, call: &str) -> Option<String> {
    let v = h.history();
    let m = v.as_array()?.iter().find(|m| m["id"] == message)?;
    m["toolCalls"]
        .as_array()?
        .iter()
        .find(|c| c["id"] == call)
        .map(|c| c["function"]["arguments"].as_str().unwrap().to_owned())
}

#[test]
fn duplicate_call_id_survives_dropping_first_holder() {
    let mut h = H::new();
    h.push(json!({"type": "MESSAGES_SNAPSHOT", "messages":
        [assistant_with("A", Some("c")), assistant_with("B", Some("c"))]}));
    h.push(json!({"type": "MESSAGES_SNAPSHOT", "messages": [assistant_with("B", Some("c"))]}));
    h.push(json!({"type": "TOOL_CALL_ARGS", "toolCallId": "c", "delta": "x"}));
    assert_eq!(args_of(&h, "B", "c").as_deref(), Some("x"));
    assert!(h.codes().is_empty());
}

#[test]
fn duplicate_call_id_survives_swapping_first_holder() {
    let mut h = H::new();
    h.push(json!({"type": "MESSAGES_SNAPSHOT", "messages":
        [assistant_with("A", Some("c")), assistant_with("B", Some("c"))]}));
    // B is restated first (c stays on A), then A loses its call.
    h.push(json!({"type": "MESSAGES_SNAPSHOT", "messages":
        [assistant_with("B", Some("c")), assistant_with("A", None)]}));
    h.push(json!({"type": "TOOL_CALL_ARGS", "toolCallId": "c", "delta": "x"}));
    assert_eq!(args_of(&h, "B", "c").as_deref(), Some("x"));
    assert!(h.codes().is_empty());
    assert_eq!(h.history().to_string().matches("\"id\":\"c\"").count(), 1);
}

#[test]
fn nameless_first_chunk_reports_missing_target() {
    let mut h = H::new();
    h.push(json!({"type": "TOOL_CALL_CHUNK", "toolCallId": "x", "delta": "a"}));
    assert_eq!(h.codes(), ["stream.missing_target"]);
    assert_eq!(h.history(), json!([]));
}

fn activities(n: usize) -> Vec<Message> {
    serde_json::from_value(Value::Array(
        (0..n)
            .map(|i| {
                json!({"id": format!("a-{i}"), "role": "activity", "activityType": "t",
                "content": {}})
            })
            .collect(),
    ))
    .unwrap()
}

/// Swapping a held activity into a class-0 message and dropping it in the
/// next snapshot walks and moves the whole held tail each time: tiny frames
/// must not buy unbounded work.
#[test]
fn cross_class_swap_drops_are_bounded() {
    let n = 4096;
    let mut h = H::from(Reducer::with_history(activities(n), json!({})));
    let mut fatal_at = None;
    for i in 0..n {
        h.push(json!({"type": "MESSAGES_SNAPSHOT", "messages":
            [{"id": format!("a-{i}"), "role": "user", "content": ""}]}));
        h.push(json!({"type": "MESSAGES_SNAPSHOT", "messages": []}));
        if h.r.take_fatal().is_some() {
            fatal_at = Some(i);
            break;
        }
    }
    let at = fatal_at.expect("the work bound must end the stream");
    assert!(at < n / 2, "ended only at pair {at}");
}

/// One large legitimate drop compacts fully, with no verdict.
#[test]
fn one_large_drop_still_compacts() {
    let mut held = vec![serde_json::from_value::<Message>(user("u0")).unwrap()];
    held.extend(activities(50_000));
    let mut h = H::from(Reducer::with_history(held, json!({})));
    h.push(json!({"type": "MESSAGES_SNAPSHOT", "messages": []}));
    assert!(h.r.take_fatal().is_none());
    assert_eq!(h.r.messages().len(), 50_000);
    assert!(h.codes().is_empty());
}

/// A restored history pays for compacting itself, however long it is, and
/// the bank is uncapped: work the input paid for stays available.
#[test]
fn restored_history_funds_its_own_compaction() {
    let r = Reducer::with_history(activities(100), json!({}));
    assert_eq!(r.reconcile_bank, RECONCILE_BANK + 100 * RECONCILE_REFILL);
    let mut h = H::from(r);
    h.r.reconcile_bank = 0;
    // Out of work: the drop is refused, reported, and fatal.
    h.push(json!({"type": "MESSAGES_SNAPSHOT", "messages":
        [{"id": "a-3", "role": "user", "content": ""}]}));
    h.push(json!({"type": "MESSAGES_SNAPSHOT", "messages": []}));
    assert_eq!(h.r.messages().len(), 100);
    assert_eq!(h.codes(), ["snapshot.work_bound"]);
    assert!(h.r.take_fatal().is_some());
}

/// A snapshot refused at the reconcile work bound is all-or-nothing: the
/// replaces and appends are not applied either (LIFE-19: the partial history
/// is what was delivered).
#[test]
fn snapshot_refused_at_work_bound_changes_nothing() {
    let mut held: Vec<Message> = serde_json::from_value(Value::Array(vec![
        json!({"id": "u0", "role": "user", "content": "x"}),
    ]))
    .unwrap();
    held.extend(activities(100));
    held.extend(
        serde_json::from_value::<Vec<Message>>(Value::Array(vec![
            json!({"id": "u1", "role": "user", "content": "x"}),
        ]))
        .unwrap(),
    );
    let mut h = H::from(Reducer::with_history(held, json!({})));
    h.r.reconcile_bank = 0;
    let before = h.r.messages().to_vec();
    let changes = h.push(json!({"type": "MESSAGES_SNAPSHOT", "messages": [
        {"id": "u1", "role": "user", "content": "changed"},
        {"id": "n1", "role": "user", "content": "new"}]}));
    assert!(
        !changes.messages_replaced,
        "a refused snapshot replaced nothing"
    );
    assert_eq!(h.codes(), ["snapshot.work_bound"]);
    assert!(h.r.take_fatal().is_some());
    assert_eq!(h.r.messages().len(), 102, "no append");
    assert_eq!(h.r.messages(), &before[..], "history untouched");
}

/// Security audit S1: a patch's credit never exceeds
/// what the content it frees was charged, so add/remove cycles cannot drain
/// the charge for content that is still held.
#[test]
fn add_remove_cycles_never_drain_the_snapshot_charge() {
    let mut h = H::new();
    h.push(json!({"type": "RUN_STARTED", "threadId": "t", "runId": "r"}));
    let members: serde_json::Map<String, Value> =
        (0..50).map(|k| (format!("k{k}"), json!(0))).collect();
    h.push(json!({
        "type": "ACTIVITY_SNAPSHOT", "messageId": "a", "activityType": "t",
        "content": {"o": Value::Object(members), "s": "x".repeat(4096)},
    }));
    let held = h.r.history_growth;
    assert!(held > 4096, "{held}");
    let delta = |patch: Value| json!({"type": "ACTIVITY_DELTA", "messageId": "a", "activityType": "t", "patch": patch});
    for i in 0..300 {
        let patches = [
            // A member added to, then removed from, a held object.
            json!([{"op": "add", "path": "/o/x", "value": 0}]),
            json!([{"op": "remove", "path": "/o/x"}]),
            // An object built member by member, then removed whole.
            json!([{"op": "add", "path": "/p", "value": {}}]),
            json!([{"op": "add", "path": "/p/a", "value": 0},
                   {"op": "add", "path": "/p/b", "value": {}},
                   {"op": "add", "path": "/p/b/c", "value": [1, 2]}]),
            json!([{"op": "remove", "path": "/p"}]),
            // An object added whole, a member moved out, then both removed.
            json!([{"op": "add", "path": "/q", "value": {"a": 0, "b": 1, "c": 2}},
                   {"op": "move", "from": "/q/a", "path": "/m"}]),
            json!([{"op": "remove", "path": "/q"}, {"op": "remove", "path": "/m"}]),
            // A value written into a held object, part of it moved out, the
            // rest freed, all in one patch; then the moved part removed.
            json!([{"op": "add", "path": "/o/x", "value": {"q": [1, 2, 3, 4, 5, 6, 7, 8], "r": "y"}},
                   {"op": "move", "from": "/o/x/q", "path": "/m"},
                   {"op": "remove", "path": "/o/x"}]),
            json!([{"op": "remove", "path": "/m"}]),
            // Written, moved within the held object, overwritten there and
            // freed.
            json!([{"op": "add", "path": "/o/x", "value": [1, 2, 3]},
                   {"op": "move", "from": "/o/x", "path": "/o/y"},
                   {"op": "add", "path": "/o/y", "value": [4, 5]},
                   {"op": "remove", "path": "/o/y"}]),
            // Written under an object the patch created, moved into a held
            // object, freed.
            json!([{"op": "add", "path": "/n", "value": {}},
                   {"op": "add", "path": "/n/v", "value": [1, 2, 3]},
                   {"op": "move", "from": "/n/v", "path": "/o/z"},
                   {"op": "remove", "path": "/o/z"},
                   {"op": "remove", "path": "/n"}]),
        ];
        for patch in patches {
            h.push(delta(patch));
            assert!(
                h.r.history_growth >= held,
                "cycle {i}: {} fell below the {held} the held content was charged",
                h.r.history_growth
            );
        }
    }
    assert!(h.codes().is_empty());
}

/// Security audit S1: what an `add` charges for the slot it fills, the
/// matching `remove` (or `move` out) credits back, so a producer that
/// toggles a member, renames one, or keeps a rolling log holds its charge
/// steady instead of drifting it up to a false `processing.limit`.
#[test]
fn add_remove_cycles_hold_the_charge_steady() {
    let cases = [
        (
            "member of an empty object",
            json!({"o": {}}),
            vec![
                json!([{"op": "add", "path": "/o/x", "value": 0}]),
                json!([{"op": "remove", "path": "/o/x"}]),
            ],
        ),
        (
            "member of the empty content",
            json!({}),
            vec![
                json!([{"op": "add", "path": "/x", "value": 0}]),
                json!([{"op": "remove", "path": "/x"}]),
            ],
        ),
        (
            "member of a held object",
            json!({"o": {"keep": 0}}),
            vec![
                json!([{"op": "add", "path": "/o/x", "value": "step"}]),
                json!([{"op": "remove", "path": "/o/x"}]),
            ],
        ),
        (
            "both in one patch",
            json!({"o": {}}),
            vec![json!([{"op": "add", "path": "/o/x", "value": 0},
                     {"op": "remove", "path": "/o/x"}])],
        ),
        (
            "rolling log",
            json!({"log": vec![0; 200]}),
            vec![
                json!([{"op": "add", "path": "/log/-", "value": 0}]),
                json!([{"op": "remove", "path": "/log/0"}]),
            ],
        ),
        (
            "short rolling log",
            json!({"log": vec![0; 16]}),
            vec![
                json!([{"op": "add", "path": "/log/-", "value": 0}]),
                json!([{"op": "remove", "path": "/log/0"}]),
            ],
        ),
        (
            "rename",
            json!({"o": {"a": 0}}),
            vec![
                json!([{"op": "move", "from": "/o/a", "path": "/o/b"}]),
                json!([{"op": "move", "from": "/o/b", "path": "/o/a"}]),
            ],
        ),
        // A non-scalar value one patch writes and frees again.
        (
            "both in one patch, non-scalar",
            json!({"o": {}}),
            vec![
                json!([{"op": "add", "path": "/o/x", "value": {"q": [1, 2, 3, 4, 5, 6, 7, 8]}},
                     {"op": "remove", "path": "/o/x"}]),
            ],
        ),
        (
            "added twice",
            json!({"o": {}}),
            vec![
                json!([{"op": "add", "path": "/o/x", "value": {"q": [1, 2, 3, 4, 5, 6, 7, 8]}},
                     {"op": "add", "path": "/o/x", "value": {"q": [1, 2, 3, 4, 5, 6, 7, 8]}},
                     {"op": "remove", "path": "/o/x"}]),
            ],
        ),
        (
            "add, replace, remove",
            json!({"o": {}}),
            vec![
                json!([{"op": "add", "path": "/o/x", "value": {"q": [1, 2, 3, 4, 5, 6, 7, 8]}},
                     {"op": "replace", "path": "/o/x", "value": {"q": [1, 2, 3, 4, 5, 6, 7, 8]}},
                     {"op": "remove", "path": "/o/x"}]),
            ],
        ),
        (
            "move over a scratch member",
            json!({}),
            vec![
                json!([{"op": "add", "path": "/a", "value": {"q": [1, 2, 3, 4, 5, 6, 7, 8]}},
                     {"op": "add", "path": "/b", "value": {"q": [1, 2, 3, 4, 5, 6, 7, 8]}},
                     {"op": "move", "from": "/a", "path": "/b"},
                     {"op": "remove", "path": "/b"}]),
            ],
        ),
    ];
    for (name, content, patches) in cases {
        let mut h = H::new();
        h.push(json!({"type": "RUN_STARTED", "threadId": "t", "runId": "r"}));
        h.push(json!({
            "type": "ACTIVITY_SNAPSHOT", "messageId": "a", "activityType": "t",
            "content": content,
        }));
        let held = h.r.history_growth;
        for _ in 0..10_000 {
            for patch in &patches {
                h.push(json!({"type": "ACTIVITY_DELTA", "messageId": "a",
                    "activityType": "t", "patch": patch}));
            }
        }
        assert!(h.codes().is_empty(), "{name}: {:?}", h.codes());
        let drift = h.r.history_growth - held;
        assert!(
            (0..1024).contains(&drift),
            "{name}: the charge moved {drift} bytes over 10,000 cycles"
        );
    }
}

/// Measuring the value a `replace` writes must not eat the bank the old value's
/// credit needs. A rolling `replace` of a member whose walk fits the bank alone
/// (here about 40 KB, between half the bank and the bank) nets zero.
#[test]
fn rolling_replace_of_a_large_member_nets_zero() {
    for n in [33_000, 40_000, 60_000] {
        let mut h = H::new();
        h.push(json!({"type": "RUN_STARTED", "threadId": "t", "runId": "r"}));
        h.push(json!({
            "type": "ACTIVITY_SNAPSHOT", "messageId": "a", "activityType": "t",
            "content": {"text": "x".repeat(n)},
        }));
        let held = h.r.history_growth;
        for _ in 0..500 {
            h.push(
                json!({"type": "ACTIVITY_DELTA", "messageId": "a", "activityType": "t",
                "patch": [{"op": "replace", "path": "/text", "value": "y".repeat(n)}]}),
            );
        }
        assert!(h.codes().is_empty(), "{n}: {:?}", h.codes());
        let drift = h.r.history_growth - held;
        assert!(
            (0..1024).contains(&drift),
            "{n}: the charge moved {drift} bytes over 500 replaces"
        );
    }
}

/// The copies `finish_run` makes of ended, unanswered calls are charged until
/// the next run starts.
#[test]
fn pending_copies_are_charged() {
    const MIB: i64 = 1024 * 1024;
    let mut h = H::new();
    h.push(json!({"type": "RUN_STARTED", "threadId": "t", "runId": "r"}));
    for i in 0..4 {
        let call = format!("c{i}");
        h.push(json!({"type": "TOOL_CALL_START", "toolCallId": call, "toolCallName": "f"}));
        h.push(json!({"type": "TOOL_CALL_ARGS", "toolCallId": call,
            "delta": "x".repeat(1024 * 1024)}));
        h.push(json!({"type": "TOOL_CALL_END", "toolCallId": call}));
    }
    let before = h.r.history_growth;
    h.push(json!({"type": "RUN_FINISHED", "threadId": "t", "runId": "r"}));
    assert_eq!(h.r.pending_tool_calls().len(), 4);
    let finished = h.r.history_growth;
    assert!(
        finished - before >= 4 * MIB,
        "charged {}",
        finished - before
    );
    h.push(json!({"type": "RUN_STARTED", "threadId": "t", "runId": "r2"}));
    assert!(
        h.r.history_growth <= before,
        "{} after {before}",
        h.r.history_growth
    );
}

/// A result recorded before its call is (re)started this run still answers
/// it: interning `answered` must not change which calls are pending.
#[test]
fn result_then_reopen_leaves_nothing_pending() {
    let mut h = H::new();
    h.push(json!({"type": "RUN_STARTED", "threadId": "t", "runId": "r"}));
    h.push(
        json!({"type": "TOOL_CALL_RESULT", "messageId": "res", "toolCallId": "x",
        "content": "ok"}),
    );
    h.push(json!({"type": "TOOL_CALL_START", "toolCallId": "x", "toolCallName": "f"}));
    h.push(json!({"type": "TOOL_CALL_END", "toolCallId": "x"}));
    h.push(json!({"type": "RUN_FINISHED", "threadId": "t", "runId": "r"}));
    assert!(h.r.pending_tool_calls().is_empty());
}

/// The run's messages are the history's tail, and `run_message_ids` names
/// exactly them, in order: a snapshot dropping a duplicate-id carrier while
/// keeping the message that holds the id must not leave a stale entry (a
/// stale entry would make a later carrier rename miss its `run_message_ids`
/// slot and keep naming the old id).
#[test]
fn run_ids_stay_the_history_tail_after_dropping_a_duplicate() {
    let tail_ids = |h: &H| -> Vec<MessageId> {
        let messages = h.r.messages();
        let n = h.r.run_message_ids().len();
        assert!(n <= messages.len(), "more run ids than messages");
        messages[messages.len() - n..]
            .iter()
            .map(|m| message_id(m).clone())
            .collect()
    };
    let mut h = H::new();
    h.push(json!({"type": "RUN_STARTED", "threadId": "t", "runId": "r"}));
    h.push(
        json!({"type": "ACTIVITY_SNAPSHOT", "messageId": "q", "activityType": "a", "content": {}}),
    );
    h.push(json!({"type": "TOOL_CALL_START", "toolCallId": "q", "toolCallName": "f"}));
    h.push(json!({"type": "TOOL_CALL_END", "toolCallId": "q"}));
    assert_eq!(h.r.run_message_ids(), [id("q"), id("q")]);
    h.push(json!({"type": "MESSAGES_SNAPSHOT", "messages": [
        {"id": "q", "role": "activity", "activityType": "a", "content": {}}
    ]}));
    assert_eq!(h.r.messages().len(), 1);
    assert_eq!(h.r.run_message_ids(), [id("q")]);
    for i in 0..3 {
        let c = format!("c{i}");
        h.push(json!({"type": "TOOL_CALL_START", "toolCallId": c, "toolCallName": "f"}));
        h.push(json!({"type": "ACTIVITY_SNAPSHOT", "messageId": c, "activityType": "a", "content": {}}));
        assert_eq!(h.r.run_message_ids(), tail_ids(&h).as_slice());
    }
    assert_eq!(h.r.messages().len(), 7);
}

/// The remembered delta target follows the history: a snapshot that moves
/// or drops the message, a newly pushed message and a renamed carrier all
/// make the next delta look its target up again.
#[test]
fn remembered_delta_targets_follow_moves_and_drops() {
    let mut h = H::new();
    h.push(json!({"type": "TEXT_MESSAGE_START", "messageId": "a"}));
    h.push(json!({"type": "TEXT_MESSAGE_CONTENT", "messageId": "a", "delta": "1"}));
    h.push(json!({"type": "TEXT_MESSAGE_CONTENT", "messageId": "a", "delta": "2"}));
    assert!(h.r.hot_message.is_some());
    // Reordered by a snapshot: `a` moves to position 1.
    h.push(json!({"type": "MESSAGES_SNAPSHOT", "messages": [
        {"id": "b", "role": "user", "content": "u"},
        {"id": "a", "role": "assistant", "content": "12"},
    ]}));
    assert!(h.r.hot_message.is_none());
    h.push(json!({"type": "TEXT_MESSAGE_CONTENT", "messageId": "a", "delta": "3"}));
    h.push(json!({"type": "TEXT_MESSAGE_CONTENT", "messageId": "a", "delta": "4"}));
    // Dropped by a snapshot: the next delta has no target.
    h.push(json!({"type": "MESSAGES_SNAPSHOT", "messages": [
        {"id": "b", "role": "user", "content": "u"},
    ]}));
    let c = h.push(json!({"type": "TEXT_MESSAGE_CONTENT", "messageId": "a", "delta": "5"}));
    assert_eq!(c.message, None);
    assert_eq!(h.codes(), ["stream.missing_target"]);
    assert_eq!(
        h.history(),
        json!([{"id": "b", "role": "user", "content": "u"}])
    );

    // Arguments: the call's slot moves when a snapshot restates its holder.
    h.push(json!({"type": "TOOL_CALL_START", "toolCallId": "c", "toolCallName": "f", "parentMessageId": "b"}));
    h.push(json!({"type": "TOOL_CALL_ARGS", "toolCallId": "c", "delta": "{"}));
    h.push(json!({"type": "TOOL_CALL_ARGS", "toolCallId": "c", "delta": "\"x\""}));
    assert!(h.r.hot_call.is_some());
    let held = h.history();
    let parent = held.as_array().unwrap().last().unwrap().clone();
    h.push(json!({"type": "MESSAGES_SNAPSHOT", "messages": [
        {"id": "z", "role": "user", "content": "first"},
        {"id": "b", "role": "user", "content": "u"},
        parent,
    ]}));
    h.push(json!({"type": "TOOL_CALL_ARGS", "toolCallId": "c", "delta": ":1"}));
    h.push(json!({"type": "TOOL_CALL_ARGS", "toolCallId": "c", "delta": "}"}));
    h.push(json!({"type": "TOOL_CALL_END", "toolCallId": "c"}));
    let held = h.history();
    let calls: Vec<&Value> = held
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|m| m.get("toolCalls"))
        .collect();
    assert_eq!(calls.len(), 1, "{held}");
    assert_eq!(calls[0][0]["function"]["arguments"], "{\"x\":1}");
    assert!(h.codes().is_empty());
}

/// `*_END` releases the growth slack of the text and arguments it closes,
/// so the history holds what it is charged for (D14), and the charge is
/// the same with or without the slack.
#[test]
fn ends_release_text_and_argument_slack() {
    let mut h = H::new();
    h.push(json!({"type": "TEXT_MESSAGE_START", "messageId": "m"}));
    h.push(json!({"type": "REASONING_MESSAGE_START", "messageId": "r", "role": "reasoning"}));
    h.push(json!({"type": "TOOL_CALL_START", "toolCallId": "c", "toolCallName": "f", "parentMessageId": "m"}));
    for i in 0..10 {
        h.push(json!({"type": "TEXT_MESSAGE_CONTENT", "messageId": "m", "delta": format!("text {i} ")}));
        h.push(json!({"type": "REASONING_MESSAGE_CONTENT", "messageId": "r", "delta": format!("why {i} ")}));
        h.push(json!({"type": "TOOL_CALL_ARGS", "toolCallId": "c", "delta": format!("{i},")}));
    }
    let charged = h.r.charged.clone();
    h.push(json!({"type": "TEXT_MESSAGE_END", "messageId": "m"}));
    h.push(json!({"type": "REASONING_MESSAGE_END", "messageId": "r"}));
    h.push(json!({"type": "TOOL_CALL_END", "toolCallId": "c"}));
    assert_eq!(h.r.charged, charged, "shrinking charges nothing");
    let Message::Assistant(m) = &h.r.messages()[0] else {
        panic!("assistant")
    };
    let text = m.content.as_ref().unwrap();
    assert_eq!(text.capacity(), text.len());
    let args = &m.tool_calls.as_ref().unwrap()[0].function.arguments;
    assert_eq!(args.capacity(), args.len());
    let Message::Reasoning(r) = &h.r.messages()[1] else {
        panic!("reasoning")
    };
    assert_eq!(r.content.capacity(), r.content.len());
    assert!(h.codes().is_empty());
}

/// The bounded count behind the activity credit refill is the full count
/// whenever that fits under the bound, and above the bound otherwise, so
/// `min(bank + 8 * size, CREDIT_BANK)` comes out the same either way.
#[test]
fn bounded_wire_count_refills_the_bank_as_the_full_count_does() {
    use crate::json::{wire_bytes, wire_bytes_upto};
    let patch: Vec<PatchOperation> = serde_json::from_value(json!([
        {"op": "add", "path": "/rows/-", "value": {"id": 1, "name": "row \"one\"", "done": false}},
        {"op": "replace", "path": "/n", "value": 12345.5},
        {"op": "remove", "path": "/old"},
    ]))
    .unwrap();
    let full = wire_bytes(&patch);
    for limit in 0..full + 4 {
        let n = wire_bytes_upto(&patch, limit);
        if full <= limit {
            assert_eq!(n, full, "limit {limit}");
        } else {
            assert!(n > limit, "limit {limit}: {n}");
        }
        for bank in [
            0,
            CREDIT_BANK / 2,
            CREDIT_BANK - 8 * limit,
            CREDIT_BANK - 1,
            CREDIT_BANK,
        ] {
            let room = CREDIT_BANK - bank;
            let bounded = bank
                .saturating_add(wire_bytes_upto(&patch, room.div_ceil(8)).saturating_mul(8))
                .min(CREDIT_BANK);
            let whole = bank.saturating_add(full * 8).min(CREDIT_BANK);
            assert_eq!(bounded, whole, "bank {bank}");
        }
    }
}

/// Metadata merged key by key is charged exactly what the merged map is
/// estimated to hold: new keys, overwritten values of other sizes and
/// repeats included.
#[test]
fn metadata_merges_charge_what_the_merged_map_holds() {
    let mut h = H::new();
    h.push(json!({"type": "TEXT_MESSAGE_START", "messageId": "m", "metadata": {"x.a": 1}}));
    let merges = [
        json!({"x.a": "a longer string value than before", "x.b": [1, 2, 3]}),
        json!({"x.b": null, "x.c": {"k": "v"}}),
        json!({"x.a": 2}),
        json!({"x.d": "d", "x.e": "e", "x.f": "f", "x.g": "g", "x.h": "h", "x.i": "i",
               "x.j": "j", "x.k": "k", "x.l": "l", "x.m": "m", "x.n": "n"}),
        json!({"x.c": {"k": "v", "k2": [true]}}),
    ];
    let empty = message_bytes(&h.r.messages()[0]);
    assert_eq!(h.r.charged[0], empty);
    for metadata in merges {
        h.push(json!({"type": "TEXT_MESSAGE_END", "messageId": "m", "metadata": metadata}));
        assert_eq!(h.r.charged[0], message_bytes(&h.r.messages()[0]));
    }
    let held = h.history();
    assert_eq!(held[0]["metadata"]["x.a"], 2);
    assert_eq!(held[0]["metadata"]["x.b"], Value::Null);
    assert_eq!(held[0]["metadata"]["x.c"]["k2"][0], true);
}

/// The allocation-free pointer lookup behind the activity credit answers
/// exactly as `serde_json`'s `Value::pointer`, escapes, odd indices and
/// malformed text included.
#[test]
fn pointer_lookup_answers_as_serde_json_does() {
    let doc = json!({"a": {"b/c": [10, {"~": 1, "~1": 2, "d~e": 3}], "": {"": 4}},
                     "0": "zero", "x~y": [5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15]});
    let pointers = [
        "",
        "/",
        "a",
        "/a",
        "/a/b~1c",
        "/a/b~1c/0",
        "/a/b~1c/1/~0",
        "/a/b~1c/1/~01",
        "/a/b~1c/1/d~0e",
        "/a/b~1c/1/~2",
        "/a/b~1c/1/~",
        "/a//",
        "/a/",
        "/0",
        "/x~0y/10",
        "/x~0y/01",
        "/x~0y/+1",
        "/x~0y/-",
        "/x~0y/1e1",
        "/x~0y/99",
        "/x~0y/18446744073709551616",
        "/a/b~1c/-1",
        "//",
        "/~1",
    ];
    for p in pointers {
        assert_eq!(crate::json::pointer(&doc, p), doc.pointer(p), "{p:?}");
    }
}
