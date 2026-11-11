//! Subagent attribution, ownership and lifecycle, consumer side
//! (`events/subagents.mdx`).

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support_pipeline;

use agui_sdk::diagnostics::DiagnosticCode;
use agui_sdk::ErrorCode;
use serde_json::{json, Value};
use support_pipeline::{errored, finished, in_run, run, started};

fn announce(id: &str) -> Value {
    json!({"type": "SUBAGENT_STARTED", "subagentRunId": id, "name": "worker"})
}

fn done(id: &str) -> Value {
    json!({"type": "SUBAGENT_FINISHED", "subagentRunId": id})
}

fn tag(mut event: Value, id: &str) -> Value {
    event["subagentRunId"] = json!(id);
    event
}

fn start(id: &str) -> Value {
    json!({"type": "TEXT_MESSAGE_START", "messageId": id, "role": "assistant"})
}

fn content(id: &str, delta: &str) -> Value {
    json!({"type": "TEXT_MESSAGE_CONTENT", "messageId": id, "delta": delta})
}

fn end(id: &str) -> Value {
    json!({"type": "TEXT_MESSAGE_END", "messageId": id})
}

/// covers: SUB-04
#[test]
fn a_suspended_invocation_id_reappears_in_a_later_run() {
    run(&[
        started("r1"),
        announce("s1"),
        json!({"type": "SUBAGENT_FINISHED", "subagentRunId": "s1", "outcome": {"type": "suspended"}}),
        finished("r1"),
        started("r2"),
        announce("s1"),
        done("s1"),
        finished("r2"),
    ])
    .assert_ok();
}

/// covers: SUB-05
#[test]
fn a_tag_on_a_standalone_event_is_provenance() {
    let r = run(&in_run(&[
        tag(json!({"type": "STEP_STARTED", "stepName": "s"}), "s1"),
        tag(json!({"type": "STEP_FINISHED", "stepName": "s"}), "s1"),
        tag(json!({"type": "CUSTOM", "name": "n", "value": 1}), "s1"),
        tag(json!({"type": "RAW", "event": {}}), "s1"),
        tag(
            json!({"type": "STATE_SNAPSHOT", "snapshot": {"by": "sub"}}),
            "s1",
        ),
    ]));
    r.assert_ok();
    assert_eq!(r.state, json!({"by": "sub"}));
    assert_eq!(r.wire()[3]["subagentRunId"], "s1");
}

/// covers: SUB-11, SUB-12
#[test]
fn attribution_without_announcement_is_accepted() {
    let r = run(&in_run(&[
        tag(start("m1"), "s1"),
        tag(content("m1", "grouped"), "s1"),
        tag(end("m1"), "s1"),
    ]));
    r.assert_ok();
    assert!(r.diagnostics.is_empty());
    assert_eq!(r.message("m1")["subagentRunId"], "s1");
}

/// covers: SUB-14
#[test]
fn subagent_error_ends_the_invocation_not_the_run() {
    let r = run(&in_run(&[
        announce("s1"),
        json!({"type": "SUBAGENT_ERROR", "subagentRunId": "s1", "message": "failed", "code": "E"}),
        start("m1"),
        content("m1", "the parent carries on"),
        end("m1"),
    ]));
    r.assert_ok();
    assert_eq!(r.message("m1")["content"], "the parent carries on");
}

/// covers: SUB-15
#[test]
fn whole_run_usage_rides_the_terminal_not_subagent_events() {
    let mut terminal = finished("r1");
    terminal["usage"] =
        json!([{"provider": "p", "model": "m", "inputTokens": 5, "outputTokens": 1}]);
    let r = run(&[
        started("r1"),
        announce("s1"),
        json!({"type": "SUBAGENT_FINISHED", "subagentRunId": "s1", "usage": {"inputTokens": 3}}),
        terminal,
    ]);
    r.assert_ok();
    let wire = r.wire();
    assert!(
        wire[2].get("usage").is_none(),
        "subagent events carry no usage"
    );
    assert!(r.warned(DiagnosticCode::UnknownField));
    assert_eq!(wire[3]["usage"][0]["inputTokens"], 5);
}

/// covers: SUB-16
#[test]
fn subagent_finished_outcomes_are_delivered() {
    let r = run(&in_run(&[
        announce("s1"),
        done("s1"),
        announce("s2"),
        json!({"type": "SUBAGENT_FINISHED", "subagentRunId": "s2",
               "outcome": {"type": "suspended", "interruptIds": ["i1"]}}),
        announce("s3"),
        json!({"type": "SUBAGENT_FINISHED", "subagentRunId": "s3", "outcome": {"type": "success"}}),
    ]));
    r.assert_ok();
    let wire = r.wire();
    assert!(wire[2].get("outcome").is_none());
    assert_eq!(wire[4]["outcome"]["type"], "suspended");
    assert_eq!(wire[4]["outcome"]["interruptIds"], json!(["i1"]));
    assert_eq!(wire[6]["outcome"]["type"], "success");
}

/// covers: SUB-20, SUB-25
#[test]
fn an_untagged_continuation_continues_what_the_opener_owns() {
    let r = run(&in_run(&[
        announce("s1"),
        tag(start("m1"), "s1"),
        content("m1", "untagged "),
        tag(content("m1", "tagged"), "s1"),
        end("m1"),
        tag(
            json!({"type": "TOOL_CALL_START", "toolCallId": "t1", "toolCallName": "f"}),
            "s1",
        ),
        json!({"type": "TOOL_CALL_ARGS", "toolCallId": "t1", "delta": "{}"}),
        json!({"type": "TOOL_CALL_END", "toolCallId": "t1"}),
        done("s1"),
    ]));
    r.assert_ok();
    assert_eq!(r.message("m1")["content"], "untagged tagged");
}

/// covers: SUB-21, SUB-24
#[test]
fn a_continuation_disagreeing_with_its_opener_is_rejected() {
    let cases = [
        vec![
            tag(
                json!({"type": "TOOL_CALL_START", "toolCallId": "tc-1", "toolCallName": "lookup"}),
                "s-1",
            ),
            json!({"type": "TOOL_CALL_ARGS", "toolCallId": "tc-1", "delta": "{}"}),
            tag(
                json!({"type": "TOOL_CALL_END", "toolCallId": "tc-1"}),
                "s-2",
            ),
        ],
        vec![tag(start("m1"), "s1"), tag(content("m1", "x"), "s2")],
        // A parent-opened message continued under a subagent's tag.
        vec![start("m1"), tag(content("m1", "x"), "s1")],
        vec![
            tag(
                json!({"type": "REASONING_MESSAGE_START", "messageId": "r1", "role": "reasoning"}),
                "s1",
            ),
            tag(
                json!({"type": "REASONING_MESSAGE_END", "messageId": "r1"}),
                "s2",
            ),
        ],
        vec![
            tag(json!({"type": "REASONING_START", "messageId": "sp"}), "s1"),
            tag(json!({"type": "REASONING_END", "messageId": "sp"}), "s2"),
        ],
        vec![
            tag(
                json!({"type": "ACTIVITY_SNAPSHOT", "messageId": "a1", "activityType": "PLAN", "content": {}}),
                "s1",
            ),
            tag(
                json!({"type": "ACTIVITY_DELTA", "messageId": "a1", "activityType": "PLAN", "patch": []}),
                "s2",
            ),
        ],
        // A tool call under one owner on a message of another.
        vec![
            tag(start("m1"), "s1"),
            tag(
                json!({"type": "TOOL_CALL_START", "toolCallId": "t1", "toolCallName": "f", "parentMessageId": "m1"}),
                "s2",
            ),
        ],
        // A reopen under another owner within the run.
        vec![tag(start("m1"), "s1"), end("m1"), tag(start("m1"), "s2")],
        // Bare chunk continuations (no delta, no metadata) synthesize
        // nothing, so the chunk expander is the only check that sees them.
        vec![
            tag(
                json!({"type": "TEXT_MESSAGE_CHUNK", "messageId": "m1", "delta": "x"}),
                "s1",
            ),
            tag(
                json!({"type": "TEXT_MESSAGE_CHUNK", "messageId": "m1"}),
                "s2",
            ),
        ],
        vec![
            tag(
                json!({"type": "TOOL_CALL_CHUNK", "toolCallId": "t1", "toolCallName": "f"}),
                "s1",
            ),
            tag(json!({"type": "TOOL_CALL_CHUNK", "toolCallId": "t1"}), "s2"),
        ],
    ];
    for body in cases {
        let r = run(&in_run(&body));
        r.assert_rejected("does not match");
        assert_eq!(
            r.error.as_ref().map(|e| e.code()),
            Some(ErrorCode::Ownership),
            "{body:?}"
        );
    }
}

/// covers(client): SUB-24, SUB-25
#[test]
fn an_untagged_tool_call_takes_its_parent_messages_owner() {
    // The inherited owner is the call's: a continuation tagged to it agrees,
    // whether the parent is a text or a reasoning message.
    for (open, close) in [
        (tag(start("m1"), "s1"), end("m1")),
        (
            tag(
                json!({"type": "REASONING_MESSAGE_START", "messageId": "m1", "role": "reasoning"}),
                "s1",
            ),
            json!({"type": "REASONING_MESSAGE_END", "messageId": "m1"}),
        ),
    ] {
        let r = run(&in_run(&[
            announce("s1"),
            open.clone(),
            json!({"type": "TOOL_CALL_START", "toolCallId": "t1", "toolCallName": "f", "parentMessageId": "m1"}),
            tag(
                json!({"type": "TOOL_CALL_ARGS", "toolCallId": "t1", "delta": "{}"}),
                "s1",
            ),
            tag(json!({"type": "TOOL_CALL_END", "toolCallId": "t1"}), "s1"),
            close,
            done("s1"),
        ]));
        r.assert_ok();
        // ... and one tagged to another invocation disagrees.
        let r = run(&in_run(&[
            announce("s1"),
            open,
            json!({"type": "TOOL_CALL_START", "toolCallId": "t1", "toolCallName": "f", "parentMessageId": "m1"}),
            tag(json!({"type": "TOOL_CALL_END", "toolCallId": "t1"}), "s2"),
        ]));
        r.assert_rejected("does not match");
    }
}

/// covers: SUB-23
#[test]
fn steps_are_per_owner() {
    run(&in_run(&[
        json!({"type": "STEP_STARTED", "stepName": "plan"}),
        tag(json!({"type": "STEP_STARTED", "stepName": "plan"}), "s1"),
        json!({"type": "STEP_FINISHED", "stepName": "plan"}),
        tag(json!({"type": "STEP_FINISHED", "stepName": "plan"}), "s1"),
    ]))
    .assert_ok();
    run(&in_run(&[
        tag(json!({"type": "STEP_STARTED", "stepName": "plan"}), "s1"),
        json!({"type": "STEP_FINISHED", "stepName": "plan"}),
    ]))
    .assert_rejected("that was not started");
}

/// covers: SUB-27
#[test]
fn nested_invocations_name_an_announced_parent() {
    run(&in_run(&[
        announce("outer"),
        json!({"type": "SUBAGENT_STARTED", "subagentRunId": "inner", "name": "n", "parentSubagentRunId": "outer"}),
        done("inner"),
        done("outer"),
    ]))
    .assert_ok();
    let r = run(&in_run(&[
        json!({"type": "SUBAGENT_STARTED", "subagentRunId": "inner", "name": "n", "parentSubagentRunId": "ghost"}),
    ]));
    r.assert_rejected("'ghost' has not been announced");
    assert_eq!(
        r.error.as_ref().map(|e| e.code()),
        Some(ErrorCode::Subagent)
    );
}

/// covers: SUB-29, SUB-30
#[test]
fn parallel_invocations_are_tracked_separately() {
    let r = run(&in_run(&[
        announce("s1"),
        announce("s2"),
        tag(start("a"), "s1"),
        tag(start("b"), "s2"),
        content("a", "1"),
        content("b", "x"),
        tag(
            json!({"type": "TEXT_MESSAGE_CHUNK", "messageId": "c", "delta": "chunk"}),
            "s2",
        ),
        end("a"),
        done("s1"),
        // s1 ending closes none of s2's entities.
        content("b", "y"),
        tag(json!({"type": "TEXT_MESSAGE_CHUNK", "delta": "ed"}), "s2"),
        end("b"),
        done("s2"),
    ]));
    r.assert_ok();
    assert_eq!(r.message("b")["content"], "xy");
    assert_eq!(r.message("c")["content"], "chunked");
}

/// covers: SUB-31, SUB-32
#[test]
fn subagent_state_updates_the_run_state() {
    let r = run(&in_run(&[
        json!({"type": "STATE_SNAPSHOT", "snapshot": {"n": 0}}),
        tag(
            json!({"type": "STATE_DELTA", "delta": [{"op": "replace", "path": "/n", "value": 1}]}),
            "s1",
        ),
        tag(
            json!({"type": "STATE_DELTA", "delta": [{"op": "add", "path": "/m", "value": 2}]}),
            "s2",
        ),
    ]));
    r.assert_ok();
    assert_eq!(r.state, json!({"n": 1, "m": 2}));
}

/// covers: SUB-34
#[test]
fn run_error_abandons_open_invocations() {
    run(&[
        started("r1"),
        announce("s1"),
        tag(start("m1"), "s1"),
        errored("boom"),
        started("r2"),
        finished("r2"),
    ])
    .assert_ok();
}

/// covers: SUB-35
#[test]
fn interrupts_keep_their_attribution() {
    let mut terminal = finished("r1");
    terminal["outcome"] = json!({"type": "interrupt", "interrupts": [
        {"id": "i1", "reason": "approval", "subagentRunId": "s1"},
        {"id": "i2", "reason": "input"}
    ]});
    let r = run(&[
        started("r1"),
        announce("s1"),
        json!({"type": "SUBAGENT_FINISHED", "subagentRunId": "s1", "outcome": {"type": "suspended", "interruptIds": ["i1"]}}),
        terminal,
    ]);
    r.assert_ok();
    let interrupts = &r.wire()[3]["outcome"]["interrupts"];
    assert_eq!(interrupts[0]["subagentRunId"], "s1");
    assert!(interrupts[1].get("subagentRunId").is_none());
}

/// covers: SUB-36
#[test]
fn a_terminal_for_an_unannounced_invocation_is_fatal() {
    let r = run(&in_run(&[
        json!({"type": "SUBAGENT_FINISHED", "subagentRunId": "s-unstarted-1"}),
    ]));
    r.assert_rejected("s-unstarted-1");
    r.assert_rejected("that was not started");
    assert_eq!(
        r.error.as_ref().map(|e| e.code()),
        Some(ErrorCode::Subagent)
    );
    run(&in_run(&[
        json!({"type": "SUBAGENT_ERROR", "subagentRunId": "s2", "message": "x"}),
    ]))
    .assert_rejected("'s2' that was not started");
    // Attribution without announcement does not make a terminal legal.
    run(&in_run(&[
        tag(json!({"type": "CUSTOM", "name": "n", "value": 1}), "s3"),
        done("s3"),
    ]))
    .assert_rejected("'s3' that was not started");
    // Nor does a second terminal.
    run(&in_run(&[announce("s4"), done("s4"), done("s4")]))
        .assert_rejected("'s4' that was not started");
    // And a run may not finish with one active.
    run(&in_run(&[announce("s-active-1")])).assert_rejected(
        "Cannot send 'RUN_FINISHED' while subagents are still active: 's-active-1'",
    );
    // Nor may an active or used id be announced again.
    run(&in_run(&[announce("s5"), announce("s5")])).assert_rejected("'s5' is already active");
    run(&in_run(&[announce("s6"), done("s6"), announce("s6")]))
        .assert_rejected("'s6' already finished in this run");
}
