//! Where each event's metadata lands and how it merges (`basic/metadata.mdx`).

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support_pipeline;
mod support_reducer;

use agui_sdk::Event;
use serde_json::json;
use support_reducer::{event, run_finished, run_started, Harness};

/// covers: META-06, META-07, META-08
#[test]
fn text_message_metadata_merges_shallow_last_write_wins() {
    let mut h = Harness::new();
    h.push(json!({"type": "TEXT_MESSAGE_START", "messageId": "m1",
        "metadata": {"a": 1, "obj": {"x": 1, "y": 2}, "arr": [1, 2]}}));
    h.push(
        json!({"type": "TEXT_MESSAGE_CONTENT", "messageId": "m1", "delta": "hi",
        "metadata": {"b": 2, "obj": {"z": 3}}}),
    );
    h.push(json!({"type": "TEXT_MESSAGE_END", "messageId": "m1",
        "metadata": {"a": "last", "arr": [9]}}));
    assert_eq!(
        h.history()[0]["metadata"],
        json!({"a": "last", "b": 2, "obj": {"z": 3}, "arr": [9]})
    );
    assert!(h.codes().is_empty());
}

/// covers: META-01, META-03, META-17
#[test]
fn any_key_and_null_values_are_kept() {
    let mut h = Harness::new();
    let meta = json!({"vendor.unknown": {"deep": [1, null]}, "nothing": null, "": 0});
    h.push(json!({"type": "TEXT_MESSAGE_START", "messageId": "m1", "metadata": meta}));
    h.push(json!({"type": "TEXT_MESSAGE_END", "messageId": "m1", "metadata": {"later": null}}));
    let got = &h.history()[0]["metadata"];
    assert_eq!(got["nothing"], json!(null));
    assert!(got.as_object().unwrap().contains_key("later"));
    assert_eq!(got["vendor.unknown"], json!({"deep": [1, null]}));
    assert!(h.codes().is_empty(), "unknown keys never warn");
}

/// covers: META-04
#[test]
fn reserved_key_is_carried_untouched() {
    // `ag-ui` belongs to the protocol; the reducer neither strips nor
    // interprets it, it merges like any key.
    let mut h = Harness::new();
    h.push(
        json!({"type": "TEXT_MESSAGE_START", "messageId": "m1", "metadata": {"ag-ui": {"v": 1}}}),
    );
    h.push(json!({"type": "TEXT_MESSAGE_END", "messageId": "m1"}));
    assert_eq!(h.history()[0]["metadata"], json!({"ag-ui": {"v": 1}}));
}

/// covers: META-09
#[test]
fn reasoning_message_metadata_merges_into_the_reasoning_message() {
    let mut h = Harness::new();
    h.push(json!({"type": "REASONING_START", "messageId": "s1", "metadata": {"span": true}}));
    h.push(json!({"type": "REASONING_MESSAGE_START", "messageId": "r1", "role": "reasoning", "metadata": {"a": 1}}));
    h.push(json!({"type": "REASONING_MESSAGE_CONTENT", "messageId": "r1", "delta": "x", "metadata": {"b": 2}}));
    h.push(json!({"type": "REASONING_MESSAGE_END", "messageId": "r1", "metadata": {"a": 3}}));
    h.push(json!({"type": "REASONING_END", "messageId": "s1", "metadata": {"span": false}}));
    let history = h.history();
    assert_eq!(history.as_array().unwrap().len(), 1);
    assert_eq!(history[0]["role"], "reasoning");
    assert_eq!(history[0]["metadata"], json!({"a": 3, "b": 2}));
}

/// covers: META-10
#[test]
fn tool_call_metadata_lands_on_the_call_not_the_message() {
    let mut h = Harness::new();
    h.push(json!({"type": "TEXT_MESSAGE_START", "messageId": "a1", "metadata": {"msg": 1}}));
    h.push(json!({"type": "TEXT_MESSAGE_END", "messageId": "a1"}));
    for (id, n) in [("c1", 1), ("c2", 2)] {
        h.push(
            json!({"type": "TOOL_CALL_START", "toolCallId": id, "toolCallName": "f",
            "parentMessageId": "a1", "metadata": {"call": n, "k": "start"}}),
        );
    }
    h.push(json!({"type": "TOOL_CALL_ARGS", "toolCallId": "c2", "delta": "{}", "metadata": {"k": "args"}}));
    h.push(json!({"type": "TOOL_CALL_END", "toolCallId": "c1", "metadata": {"k": "end"}}));
    h.push(json!({"type": "TOOL_CALL_END", "toolCallId": "c2"}));
    let m = &h.history()[0];
    assert_eq!(m["metadata"], json!({"msg": 1}));
    assert_eq!(
        m["toolCalls"][0]["metadata"],
        json!({"call": 1, "k": "end"})
    );
    assert_eq!(
        m["toolCalls"][1]["metadata"],
        json!({"call": 2, "k": "args"})
    );
}

/// covers: META-11
#[test]
fn tool_result_metadata_lands_on_the_tool_message() {
    let mut h = Harness::new();
    h.push(json!({"type": "TOOL_CALL_START", "toolCallId": "c1", "toolCallName": "f", "metadata": {"on": "call"}}));
    h.push(json!({"type": "TOOL_CALL_END", "toolCallId": "c1"}));
    h.push(
        json!({"type": "TOOL_CALL_RESULT", "messageId": "t1", "toolCallId": "c1", "content": "ok",
        "metadata": {"latency": 12}}),
    );
    let history = h.history();
    assert_eq!(history[1]["role"], "tool");
    assert_eq!(history[1]["metadata"], json!({"latency": 12}));
    assert_eq!(
        history[0]["toolCalls"][0]["metadata"],
        json!({"on": "call"})
    );
}

/// covers: META-12
#[test]
fn activity_metadata_survives_a_replacing_snapshot() {
    let mut h = Harness::new();
    h.push(json!({"type": "ACTIVITY_SNAPSHOT", "messageId": "act", "activityType": "s", "content": {"n": 0},
        "metadata": {"a": 1}}));
    h.push(
        json!({"type": "ACTIVITY_DELTA", "messageId": "act", "activityType": "s",
        "patch": [{"op": "replace", "path": "/n", "value": 1}], "metadata": {"b": 2}}),
    );
    h.push(json!({"type": "ACTIVITY_SNAPSHOT", "messageId": "act", "activityType": "s", "content": {"n": 5},
        "metadata": {"a": "new"}}));
    let m = &h.history()[0];
    assert_eq!(m["content"], json!({"n": 5}));
    assert_eq!(m["metadata"], json!({"a": "new", "b": 2}));
}

/// covers: META-13
#[test]
fn chunk_metadata_merges_like_its_expanded_events() {
    // Through the shipped pipeline: the chunks are expanded first, and the
    // metadata reaches history only through the events they stand for.
    let chunks = [
        json!({"type": "TEXT_MESSAGE_CHUNK", "messageId": "m1", "delta": "a", "metadata": {"x": 1}}),
        json!({"type": "TEXT_MESSAGE_CHUNK", "messageId": "m1", "delta": "b", "metadata": {"x": 2, "y": 1}}),
        // A metadata-only continuation still merges.
        json!({"type": "TEXT_MESSAGE_CHUNK", "messageId": "m1", "metadata": {"z": 3}}),
        json!({"type": "TOOL_CALL_CHUNK", "toolCallId": "c1", "toolCallName": "f", "parentMessageId": "m1",
            "delta": "{", "metadata": {"c": 1}}),
        json!({"type": "TOOL_CALL_CHUNK", "toolCallId": "c1", "delta": "}", "metadata": {"c": 2}}),
        json!({"type": "REASONING_MESSAGE_CHUNK", "messageId": "r1", "delta": "t", "metadata": {"r": 1}}),
    ];
    // The same content sent as plain events.
    let plain = [
        json!({"type": "TEXT_MESSAGE_START", "messageId": "m1", "role": "assistant", "metadata": {"x": 1}}),
        json!({"type": "TEXT_MESSAGE_CONTENT", "messageId": "m1", "delta": "a", "metadata": {"x": 1}}),
        json!({"type": "TEXT_MESSAGE_CONTENT", "messageId": "m1", "delta": "b", "metadata": {"x": 2, "y": 1}}),
        json!({"type": "TEXT_MESSAGE_CONTENT", "messageId": "m1", "delta": "", "metadata": {"z": 3}}),
        json!({"type": "TEXT_MESSAGE_END", "messageId": "m1"}),
        json!({"type": "TOOL_CALL_START", "toolCallId": "c1", "toolCallName": "f", "parentMessageId": "m1",
            "metadata": {"c": 1}}),
        json!({"type": "TOOL_CALL_ARGS", "toolCallId": "c1", "delta": "{", "metadata": {"c": 1}}),
        json!({"type": "TOOL_CALL_ARGS", "toolCallId": "c1", "delta": "}", "metadata": {"c": 2}}),
        json!({"type": "TOOL_CALL_END", "toolCallId": "c1"}),
        json!({"type": "REASONING_MESSAGE_START", "messageId": "r1", "role": "reasoning", "metadata": {"r": 1}}),
        json!({"type": "REASONING_MESSAGE_CONTENT", "messageId": "r1", "delta": "t", "metadata": {"r": 1}}),
        json!({"type": "REASONING_MESSAGE_END", "messageId": "r1"}),
    ];
    let chunked = support_pipeline::run(&support_pipeline::in_run(&chunks));
    chunked.assert_ok();
    let expanded = support_pipeline::run(&support_pipeline::in_run(&plain));
    expanded.assert_ok();
    let history = &chunked.messages;
    assert_eq!(history[0]["content"], "ab");
    assert_eq!(history[0]["metadata"], json!({"x": 2, "y": 1, "z": 3}));
    assert_eq!(history[0]["toolCalls"][0]["function"]["arguments"], "{}");
    assert_eq!(history[0]["toolCalls"][0]["metadata"], json!({"c": 2}));
    assert_eq!(history[1]["metadata"], json!({"r": 1}));
    assert_eq!(chunked.messages, expanded.messages);
}

/// covers: META-14, META-15
#[test]
fn events_without_a_target_merge_nowhere() {
    let mut h = Harness::with_history(
        json!([{"id": "m0", "role": "user", "content": "hi"}]),
        json!({}),
    );
    let before = h.history();
    let mut run_meta = run_finished();
    run_meta["metadata"] = json!({"finish": "stop"});
    for e in [
        {
            let mut e = run_started();
            e["metadata"] = json!({"k": 1});
            e
        },
        json!({"type": "STEP_STARTED", "stepName": "s", "metadata": {"k": 2}}),
        json!({"type": "STEP_FINISHED", "stepName": "s", "metadata": {"k": 3}}),
        json!({"type": "STATE_SNAPSHOT", "snapshot": {}, "metadata": {"k": 4}}),
        json!({"type": "RAW", "event": {}, "metadata": {"k": 5}}),
        json!({"type": "CUSTOM", "name": "acme.x", "value": 1, "metadata": {"k": 6}}),
        run_meta.clone(),
    ] {
        h.push(e);
    }
    assert_eq!(h.history(), before, "no message gained metadata");
    // The event's own metadata stays on the event for whoever observes it.
    let Event::RunFinished(f) = event(run_meta) else {
        panic!()
    };
    assert_eq!(
        serde_json::to_value(f.metadata).unwrap(),
        json!({"finish": "stop"})
    );
}

/// covers: META-16
#[test]
fn stored_metadata_is_plain_data() {
    // Metadata arriving with history is kept as is; only the listed events
    // merge into it.
    let mut h = Harness::with_history(
        json!([{"id": "a1", "role": "assistant", "content": "x", "metadata": {"old": 1},
            "toolCalls": [{"id": "c1", "type": "function", "function": {"name": "f", "arguments": ""},
                "metadata": {"stored": true}}]}]),
        json!({}),
    );
    h.push(json!({"type": "STATE_SNAPSHOT", "snapshot": {}, "metadata": {"old": 2}}));
    assert_eq!(h.history()[0]["metadata"], json!({"old": 1}));
    assert_eq!(
        h.history()[0]["toolCalls"][0]["metadata"],
        json!({"stored": true})
    );
    h.push(json!({"type": "TEXT_MESSAGE_START", "messageId": "a1", "metadata": {"new": 1}}));
    assert_eq!(h.history()[0]["metadata"], json!({"old": 1, "new": 1}));
}
