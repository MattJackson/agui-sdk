//! Chunk expansion rules (`basic/patterns/streaming.mdx`, "The chunked form").

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support_pipeline;

use agui_sdk::diagnostics::DiagnosticCode;
use agui_sdk::ErrorCode;
use serde_json::{json, Value};
use support_pipeline::{expand, in_run, run, started, types, verify};

fn chunk(id: Option<&str>, delta: &str) -> Value {
    let mut c = json!({"type": "TEXT_MESSAGE_CHUNK", "delta": delta});
    if let Some(id) = id {
        c["messageId"] = json!(id);
    }
    c
}

fn tagged(mut event: Value, subagent: &str) -> Value {
    event["subagentRunId"] = json!(subagent);
    event
}

/// covers: STRM-07
#[test]
fn chunks_reach_the_application_as_start_content_end() {
    let r = run(&in_run(&[
        chunk(Some("m1"), "Chunks "),
        chunk(None, "expand."),
    ]));
    r.assert_ok();
    assert_eq!(
        r.types(),
        [
            "RUN_STARTED",
            "TEXT_MESSAGE_START",
            "TEXT_MESSAGE_CONTENT",
            "TEXT_MESSAGE_CONTENT",
            "TEXT_MESSAGE_END",
            "RUN_FINISHED"
        ]
    );
    assert_eq!(r.message("m1")["content"], "Chunks expand.");

    let tools = run(&in_run(&[
        json!({"type": "TOOL_CALL_CHUNK", "toolCallId": "t1", "toolCallName": "f", "delta": "{}"}),
        json!({"type": "REASONING_MESSAGE_CHUNK", "messageId": "r1", "delta": "hm"}),
    ]));
    tools.assert_ok();
    assert_eq!(
        tools.types(),
        [
            "RUN_STARTED",
            "TOOL_CALL_START",
            "TOOL_CALL_ARGS",
            "TOOL_CALL_END",
            "REASONING_MESSAGE_START",
            "REASONING_MESSAGE_CONTENT",
            "REASONING_MESSAGE_END",
            "RUN_FINISHED"
        ]
    );
}

/// covers: STRM-08
#[test]
fn a_malformed_chunk_is_rejected_not_repaired() {
    let r = run(&in_run(&[
        json!({"type": "TEXT_MESSAGE_CHUNK", "messageId": "m1", "role": 7, "delta": "x"}),
    ]));
    assert_eq!(
        r.error.as_ref().map(|e| e.code()),
        Some(ErrorCode::InvalidValue)
    );
    assert!(!r.types().contains(&"TEXT_MESSAGE_START"));

    let r = run(&in_run(&[
        json!({"type": "TOOL_CALL_CHUNK", "toolCallId": 5, "toolCallName": "f"}),
    ]));
    assert_eq!(
        r.error.as_ref().map(|e| e.code()),
        Some(ErrorCode::InvalidValue)
    );
}

/// covers: STRM-10
#[test]
fn an_explicit_event_naming_the_assembling_id_ends_the_shorthand() {
    let stream = [
        started("r1"),
        chunk(Some("m1"), "half"),
        json!({"type": "TEXT_MESSAGE_CONTENT", "messageId": "m1", "delta": "more"}),
    ];
    let expanded = expand(&stream).unwrap();
    assert_eq!(
        types(&expanded),
        [
            "RUN_STARTED",
            "TEXT_MESSAGE_START",
            "TEXT_MESSAGE_CONTENT",
            "TEXT_MESSAGE_END",
            "TEXT_MESSAGE_CONTENT"
        ]
    );
    // What follows is judged by the triad rules: the message is closed.
    run(&stream).assert_rejected("No active text message found with ID 'm1'");

    // The same for a subagent's stream named by an untagged explicit event.
    let expanded = expand(&[
        tagged(chunk(Some("m2"), "a"), "s1"),
        json!({"type": "TEXT_MESSAGE_END", "messageId": "m2"}),
    ])
    .unwrap();
    assert_eq!(
        types(&expanded),
        [
            "TEXT_MESSAGE_START",
            "TEXT_MESSAGE_CONTENT",
            "TEXT_MESSAGE_END",
            "TEXT_MESSAGE_END"
        ]
    );
}

/// covers: STRM-12
#[test]
fn first_text_chunk_role_is_optional_and_defaults_to_assistant() {
    let r = run(&in_run(&[chunk(Some("m1"), "hi")]));
    r.assert_ok();
    assert_eq!(r.message("m1")["role"], "assistant");
    assert!(
        r.wire()[1].get("role").is_none(),
        "no role is invented on the start"
    );

    let r = run(&in_run(&[
        json!({"type": "TEXT_MESSAGE_CHUNK", "messageId": "m2", "role": "user", "delta": "hi"}),
    ]));
    r.assert_ok();
    assert_eq!(r.message("m2")["role"], "user");
    assert_eq!(r.wire()[1]["role"], "user");
}

/// covers: STRM-15
#[test]
fn later_chunks_omit_opener_fields_and_continue() {
    let r = run(&in_run(&[
        json!({"type": "TOOL_CALL_CHUNK", "toolCallId": "t1", "toolCallName": "search", "delta": "{\"q\":"}),
        json!({"type": "TOOL_CALL_CHUNK", "delta": "\"x\"}"}),
        json!({"type": "TOOL_CALL_CHUNK", "toolCallId": "t1", "delta": ""}),
    ]));
    r.assert_ok();
    let args: String = r
        .wire()
        .iter()
        .filter(|e| e["type"] == "TOOL_CALL_ARGS")
        .map(|e| e["delta"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(args, "{\"q\":\"x\"}");
    assert_eq!(
        r.types()
            .iter()
            .filter(|t| **t == "TOOL_CALL_START")
            .count(),
        1
    );
}

/// covers: STRM-16
#[test]
fn a_first_chunk_missing_its_required_field_is_a_violation() {
    let cases = [
        (
            json!({"type": "TEXT_MESSAGE_CHUNK", "delta": "no message is open"}),
            "First TEXT_MESSAGE_CHUNK must have a messageId",
        ),
        (
            json!({"type": "TOOL_CALL_CHUNK", "toolCallId": "t1", "delta": "{}"}),
            "First TOOL_CALL_CHUNK must have a toolCallName",
        ),
        (
            json!({"type": "TOOL_CALL_CHUNK", "toolCallName": "f", "delta": "{}"}),
            "TOOL_CALL_CHUNK cannot open a tool call: toolCallId is absent",
        ),
        (
            json!({"type": "REASONING_MESSAGE_CHUNK", "delta": "hm"}),
            "REASONING_MESSAGE_CHUNK cannot open a reasoning message: messageId is absent",
        ),
    ];
    for (bad, needle) in cases {
        let r = run(&in_run(&[bad]));
        r.assert_rejected(needle);
        assert_eq!(r.error.as_ref().map(|e| e.code()), Some(ErrorCode::Chunk));
        assert_eq!(
            r.types(),
            ["RUN_STARTED"],
            "nothing is opened under an invented id"
        );
    }
}

/// covers: STRM-18
#[test]
fn a_chunk_naming_a_different_id_closes_the_previous_item_first() {
    let r = run(&in_run(&[
        chunk(Some("m1"), "one"),
        chunk(Some("m2"), "two"),
    ]));
    r.assert_ok();
    assert_eq!(
        r.types(),
        [
            "RUN_STARTED",
            "TEXT_MESSAGE_START",
            "TEXT_MESSAGE_CONTENT",
            "TEXT_MESSAGE_END",
            "TEXT_MESSAGE_START",
            "TEXT_MESSAGE_CONTENT",
            "TEXT_MESSAGE_END",
            "RUN_FINISHED"
        ]
    );
    let wire = r.wire();
    assert_eq!(wire[3]["messageId"], "m1");
    assert_eq!(wire[4]["messageId"], "m2");
    assert_eq!(r.message("m1")["content"], "one");
    assert_eq!(r.message("m2")["content"], "two");
}

/// covers: STRM-19
#[test]
fn a_continuation_may_repeat_opener_fields_with_the_same_value() {
    let r = run(&in_run(&[
        json!({"type": "TEXT_MESSAGE_CHUNK", "messageId": "m1", "role": "assistant", "name": "bot", "delta": "a"}),
        json!({"type": "TEXT_MESSAGE_CHUNK", "messageId": "m1", "role": "assistant", "name": "bot", "delta": "b"}),
        json!({"type": "TOOL_CALL_CHUNK", "toolCallId": "t1", "toolCallName": "f", "parentMessageId": "m1", "delta": "{"}),
        json!({"type": "TOOL_CALL_CHUNK", "toolCallId": "t1", "toolCallName": "f", "parentMessageId": "m1", "delta": "}"}),
    ]));
    r.assert_ok();
    assert!(r.diagnostics.is_empty(), "{:?}", r.diagnostics);
    assert_eq!(r.message("m1")["content"], "ab");
}

/// covers: STRM-20
#[test]
fn a_conflicting_repeat_is_a_violation() {
    let cases = [
        (
            json!({"type": "TEXT_MESSAGE_CHUNK", "messageId": "m1", "role": "user", "delta": "a"}),
            json!({"type": "TEXT_MESSAGE_CHUNK", "messageId": "m1", "role": "assistant", "delta": "b"}),
            "does not match the open stream's role",
        ),
        // Established by omission: no role means assistant.
        (
            json!({"type": "TEXT_MESSAGE_CHUNK", "messageId": "m1", "delta": "a"}),
            json!({"type": "TEXT_MESSAGE_CHUNK", "role": "user", "delta": "b"}),
            "does not match the open stream's role",
        ),
        (
            json!({"type": "TEXT_MESSAGE_CHUNK", "messageId": "m1", "name": "a", "delta": "a"}),
            json!({"type": "TEXT_MESSAGE_CHUNK", "messageId": "m1", "name": "b", "delta": "b"}),
            "does not match the open stream's name",
        ),
        (
            json!({"type": "TOOL_CALL_CHUNK", "toolCallId": "t1", "toolCallName": "search", "delta": "{"}),
            json!({"type": "TOOL_CALL_CHUNK", "toolCallId": "t1", "toolCallName": "delete", "delta": "}"}),
            "does not match the open stream's toolCallName",
        ),
        (
            json!({"type": "TOOL_CALL_CHUNK", "toolCallId": "t1", "toolCallName": "f", "parentMessageId": "m1"}),
            json!({"type": "TOOL_CALL_CHUNK", "parentMessageId": "m2", "delta": "{}"}),
            "does not match the open stream's parentMessageId",
        ),
    ];
    for (first, second, needle) in cases {
        let r = run(&in_run(&[first, second]));
        r.assert_rejected(needle);
        assert_eq!(r.error.as_ref().map(|e| e.code()), Some(ErrorCode::Chunk));
    }
}

/// covers: STRM-21
#[test]
fn an_event_in_the_lane_closes_its_chunk_stream() {
    let closers = [
        json!({"type": "TEXT_MESSAGE_START", "messageId": "other"}),
        json!({"type": "TOOL_CALL_START", "toolCallId": "t9", "toolCallName": "f"}),
        json!({"type": "TOOL_CALL_RESULT", "messageId": "r9", "toolCallId": "t9", "content": "ok"}),
        json!({"type": "STEP_STARTED", "stepName": "s"}),
        json!({"type": "STATE_SNAPSHOT", "snapshot": {}}),
        json!({"type": "STATE_DELTA", "delta": []}),
        json!({"type": "CUSTOM", "name": "n", "value": 1}),
        json!({"type": "REASONING_START", "messageId": "span"}),
        json!({"type": "REASONING_MESSAGE_START", "messageId": "rm", "role": "reasoning"}),
    ];
    for closer in closers {
        let expanded = expand(&[chunk(Some("m1"), "a"), closer.clone()]).unwrap();
        assert_eq!(
            types(&expanded)[2..],
            ["TEXT_MESSAGE_END", closer["type"].as_str().unwrap()],
            "{closer}"
        );
    }
    // A closed chunk stream cannot be continued by an id-less chunk.
    run(&in_run(&[
        chunk(Some("m1"), "Half "),
        json!({"type": "CUSTOM", "name": "n", "value": {}}),
        chunk(None, "and half."),
    ]))
    .assert_rejected("First TEXT_MESSAGE_CHUNK must have a messageId");

    let bystanders = [
        json!({"type": "RAW", "event": {"k": 1}}),
        json!({"type": "ACTIVITY_SNAPSHOT", "messageId": "a1", "activityType": "PLAN", "content": {}}),
        json!({"type": "ACTIVITY_DELTA", "messageId": "a1", "activityType": "PLAN", "patch": []}),
        json!({"type": "REASONING_ENCRYPTED_VALUE", "subtype": "message", "entityId": "m1", "encryptedValue": "x"}),
        json!({"type": "SUBAGENT_STARTED", "subagentRunId": "s1", "name": "n"}),
    ];
    for bystander in bystanders {
        let expanded =
            expand(&[chunk(Some("m1"), "a"), bystander.clone(), chunk(None, "b")]).unwrap();
        assert_eq!(
            types(&expanded),
            [
                "TEXT_MESSAGE_START",
                "TEXT_MESSAGE_CONTENT",
                bystander["type"].as_str().unwrap(),
                "TEXT_MESSAGE_CONTENT"
            ],
            "{bystander}"
        );
    }
    let r = run(&in_run(&[
        chunk(Some("m1"), "Half "),
        json!({"type": "RAW", "event": {"kind": "heartbeat"}}),
        chunk(None, "and half."),
    ]));
    r.assert_ok();
    assert_eq!(r.message("m1")["content"], "Half and half.");
}

/// covers: STRM-22
#[test]
fn run_level_events_close_every_lane_and_a_subagent_terminal_closes_its_own() {
    for closer in [
        json!({"type": "RUN_FINISHED", "threadId": "t", "runId": "r"}),
        json!({"type": "RUN_ERROR", "message": "x"}),
        json!({"type": "RUN_STARTED", "threadId": "t", "runId": "r"}),
        json!({"type": "MESSAGES_SNAPSHOT", "messages": []}),
    ] {
        let expanded = expand(&[
            chunk(Some("m1"), "a"),
            tagged(chunk(Some("m2"), "b"), "s1"),
            json!({"type": "TOOL_CALL_CHUNK", "toolCallId": "t1", "toolCallName": "f", "subagentRunId": "s2"}),
            closer.clone(),
        ])
        .unwrap();
        assert_eq!(
            types(&expanded)[5..],
            [
                "TEXT_MESSAGE_END",
                "TEXT_MESSAGE_END",
                "TOOL_CALL_END",
                closer["type"].as_str().unwrap()
            ],
            "{closer}"
        );
    }
    let expanded = expand(&[
        chunk(Some("m1"), "a"),
        tagged(chunk(Some("m2"), "b"), "s1"),
        json!({"type": "SUBAGENT_FINISHED", "subagentRunId": "s1"}),
        chunk(None, "c"),
    ])
    .unwrap();
    assert_eq!(
        types(&expanded)[4..],
        [
            "TEXT_MESSAGE_END",
            "SUBAGENT_FINISHED",
            "TEXT_MESSAGE_CONTENT"
        ]
    );
    let wire = serde_json::to_value(&expanded[4]).unwrap();
    assert_eq!(wire["messageId"], "m2");
    let wire = serde_json::to_value(&expanded[6]).unwrap();
    assert_eq!(wire["messageId"], "m1", "the parent's stream survives");

    let r = run(&in_run(&[
        chunk(Some("m1"), "Chunks "),
        chunk(None, "expand."),
    ]));
    r.assert_ok();
    assert!(r.diagnostics.is_empty());
}

/// covers: STRM-23
#[test]
fn an_untagged_idless_continuation_resolves_to_parent_then_sole_stream() {
    // The parent's stream wins even when a subagent also has one open.
    let expanded = expand(&[
        tagged(chunk(Some("sub"), "s"), "s1"),
        chunk(Some("par"), "p"),
        chunk(None, "+"),
    ])
    .unwrap();
    let last = serde_json::to_value(expanded.last().unwrap()).unwrap();
    assert_eq!(last["messageId"], "par");

    // Without a parent stream, the sole open stream of the kind.
    let r = run(&in_run(&[
        json!({"type": "SUBAGENT_STARTED", "subagentRunId": "s1", "name": "n"}),
        tagged(chunk(Some("sub"), "half "), "s1"),
        chunk(None, "whole"),
        json!({"type": "SUBAGENT_FINISHED", "subagentRunId": "s1"}),
    ]));
    r.assert_ok();
    assert_eq!(r.message("sub")["content"], "half whole");

    // Only streams of the continuation's kind count.
    let expanded = expand(&[
        json!({"type": "TOOL_CALL_CHUNK", "toolCallId": "t1", "toolCallName": "f", "subagentRunId": "s1"}),
        tagged(chunk(Some("sub"), "a"), "s2"),
        json!({"type": "TOOL_CALL_CHUNK", "delta": "{}"}),
    ])
    .unwrap();
    let last = serde_json::to_value(expanded.last().unwrap()).unwrap();
    assert_eq!(last["toolCallId"], "t1");
}

/// covers: STRM-24
#[test]
fn a_continuation_without_a_unique_referent_is_ambiguous() {
    let stream = in_run(&[
        tagged(chunk(Some("a"), "x"), "s1"),
        tagged(chunk(Some("b"), "y"), "s2"),
        chunk(None, "whose?"),
    ]);
    let r = run(&stream);
    r.assert_rejected("ambiguous");
    assert_eq!(r.error.as_ref().map(|e| e.code()), Some(ErrorCode::Chunk));

    // A chunk naming an assembling item under another owner is refused by
    // expansion itself, before anything is synthesized for it.
    let err = expand(&[
        tagged(chunk(Some("a"), "x"), "s1"),
        tagged(chunk(Some("a"), "y"), "s2"),
    ])
    .unwrap_err();
    assert_eq!(err.code(), ErrorCode::Ownership);

    // Attributed continuations are fine.
    verify(&in_run(&[
        tagged(chunk(Some("a"), "x"), "s1"),
        tagged(chunk(Some("b"), "y"), "s2"),
        tagged(chunk(None, "1"), "s1"),
        tagged(chunk(None, "2"), "s2"),
    ]))
    .unwrap();
}

/// covers: STRM-26
#[test]
fn a_subagent_chunk_stream_closes_no_later_than_its_terminal() {
    let r = run(&in_run(&[
        json!({"type": "SUBAGENT_STARTED", "subagentRunId": "s1", "name": "a"}),
        json!({"type": "SUBAGENT_STARTED", "subagentRunId": "s2", "name": "b"}),
        tagged(chunk(Some("m1"), "half "), "s1"),
        tagged(chunk(Some("m2"), "other "), "s2"),
        tagged(chunk(None, "assembled"), "s1"),
        json!({"type": "SUBAGENT_ERROR", "subagentRunId": "s1", "message": "x"}),
        tagged(chunk(None, "continues"), "s2"),
        json!({"type": "SUBAGENT_FINISHED", "subagentRunId": "s2"}),
    ]));
    r.assert_ok();
    let wire = r.wire();
    let end_m1 = wire
        .iter()
        .position(|e| e["type"] == "TEXT_MESSAGE_END" && e["messageId"] == "m1")
        .unwrap();
    let terminal = wire
        .iter()
        .position(|e| e["type"] == "SUBAGENT_ERROR")
        .unwrap();
    assert_eq!(end_m1 + 1, terminal);
    assert_eq!(r.message("m1")["content"], "half assembled");
    assert_eq!(r.message("m2")["content"], "other continues");
    assert_eq!(r.message("m1")["subagentRunId"], "s1");
}

/// covers: STRM-27
#[test]
fn chunk_metadata_reaches_the_synthesized_events_and_the_item() {
    let r = run(&in_run(&[
        json!({"type": "TEXT_MESSAGE_CHUNK", "messageId": "m1", "delta": "hi", "metadata": {"a": 1}}),
        json!({"type": "TEXT_MESSAGE_CHUNK", "metadata": {"finishReason": "stop", "a": 2}}),
    ]));
    r.assert_ok();
    let wire = r.wire();
    assert_eq!(wire[1]["type"], "TEXT_MESSAGE_START");
    assert_eq!(wire[1]["metadata"]["a"], 1);
    assert_eq!(wire[2]["metadata"]["a"], 1);
    // The metadata-only continuation is legal and adds no content.
    assert_eq!(wire[3]["type"], "TEXT_MESSAGE_CONTENT");
    assert_eq!(wire[3]["delta"], "");
    assert_eq!(wire[3]["metadata"]["finishReason"], "stop");
    let message = r.message("m1");
    assert_eq!(message["content"], "hi");
    assert_eq!(message["metadata"]["finishReason"], "stop");
    assert_eq!(message["metadata"]["a"], 2);
    assert!(!r.warned(DiagnosticCode::UnknownField));
}
