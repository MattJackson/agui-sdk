//! Consumer-side tests behind compliance.toml reasons that the per-area
//! files did not already prove.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support_pipeline;

#[cfg(feature = "client")]
mod support_client;

use std::sync::Arc;

use agui_sdk::diagnostics::{CollectingSink, DiagnosticCode, Reporter};
use agui_sdk::pipeline::compat::EraShims;
use agui_sdk::version::LegacyEra;
use serde_json::{json, Value};
use support_pipeline::{expand, finished, in_run, run, started};

/// covers: BASIC-17
#[test]
fn the_tool_call_id_ties_the_call_its_result_and_its_interrupt() {
    let mut terminal = finished("r1");
    terminal["outcome"] = json!({"type": "interrupt", "interrupts": [
        {"id": "i1", "reason": "tool_approval", "toolCallId": "t2"}
    ]});
    let r = run(&[
        started("r1"),
        json!({"type": "TEXT_MESSAGE_START", "messageId": "m1", "role": "assistant"}),
        json!({"type": "TEXT_MESSAGE_END", "messageId": "m1"}),
        json!({"type": "TOOL_CALL_START", "toolCallId": "t1", "toolCallName": "lookup", "parentMessageId": "m1"}),
        json!({"type": "TOOL_CALL_END", "toolCallId": "t1"}),
        json!({"type": "TOOL_CALL_RESULT", "messageId": "res1", "toolCallId": "t1", "content": "found"}),
        json!({"type": "TOOL_CALL_START", "toolCallId": "t2", "toolCallName": "delete", "parentMessageId": "m1"}),
        json!({"type": "TOOL_CALL_END", "toolCallId": "t2"}),
        terminal,
    ]);
    r.assert_ok();
    assert_eq!(r.message("res1")["toolCallId"], "t1");
    let calls = r.message("m1")["toolCalls"].as_array().unwrap().clone();
    assert_eq!(calls[0]["id"], "t1");
    let last = r.wire().last().unwrap().clone();
    assert_eq!(last["outcome"]["interrupts"][0]["toolCallId"], "t2");
}

/// covers: STRM-15
#[test]
fn id_less_text_and_reasoning_chunks_continue_the_open_stream() {
    let r = run(&in_run(&[
        json!({"type": "TEXT_MESSAGE_CHUNK", "messageId": "m1", "delta": "Hel"}),
        json!({"type": "TEXT_MESSAGE_CHUNK", "delta": "lo"}),
        json!({"type": "REASONING_MESSAGE_CHUNK", "messageId": "r1", "delta": "thin"}),
        json!({"type": "REASONING_MESSAGE_CHUNK", "delta": "king"}),
    ]));
    r.assert_ok();
    assert_eq!(r.message("m1")["content"], "Hello");
    assert_eq!(r.message("r1")["content"], "thinking");
    let starts = r
        .types()
        .iter()
        .filter(|t| **t == "TEXT_MESSAGE_START" || **t == "REASONING_MESSAGE_START")
        .count();
    assert_eq!(starts, 2, "a continuation opens nothing new");
}

/// covers: INTR-24
#[test]
fn interrupts_on_a_success_outcome_are_stripped_with_a_warning() {
    let mut terminal = finished("r1");
    terminal["outcome"] = json!({"type": "success", "interrupts": [
        {"id": "i1", "reason": "approval"}
    ]});
    let r = run(&[started("r1"), terminal]);
    r.assert_ok();
    let last = r.wire().last().unwrap().clone();
    assert_eq!(last["outcome"], json!({"type": "success"}));
    assert!(
        r.warned(DiagnosticCode::UnknownField),
        "{:?}",
        r.diagnostics
    );
}

/// covers: REAS-10
#[test]
fn ending_an_already_closed_span_is_rejected() {
    run(&in_run(&[
        json!({"type": "REASONING_START", "messageId": "s1"}),
        json!({"type": "REASONING_END", "messageId": "s1"}),
        json!({"type": "REASONING_END", "messageId": "s1"}),
    ]))
    .assert_rejected("No active reasoning span found with ID 's1'");
}

/// covers: SUB-20
#[test]
fn untagged_reasoning_and_activity_continuations_keep_their_owner() {
    let r = run(&in_run(&[
        json!({"type": "SUBAGENT_STARTED", "subagentRunId": "s1", "name": "helper"}),
        json!({"type": "REASONING_MESSAGE_START", "messageId": "r1", "role": "reasoning", "subagentRunId": "s1"}),
        json!({"type": "REASONING_MESSAGE_CONTENT", "messageId": "r1", "delta": "hm"}),
        json!({"type": "REASONING_MESSAGE_END", "messageId": "r1"}),
        json!({"type": "ACTIVITY_SNAPSHOT", "messageId": "a1", "activityType": "PLAN", "content": {"n": 1}, "subagentRunId": "s1"}),
        json!({"type": "ACTIVITY_DELTA", "messageId": "a1", "activityType": "PLAN",
               "patch": [{"op": "replace", "path": "/n", "value": 2}]}),
        json!({"type": "SUBAGENT_FINISHED", "subagentRunId": "s1"}),
    ]));
    r.assert_ok();
    assert_eq!(r.message("r1")["content"], "hm");
    assert_eq!(r.message("a1")["content"], json!({"n": 2}));
    assert_eq!(r.message("r1")["subagentRunId"], "s1");
    assert_eq!(r.message("a1")["subagentRunId"], "s1");
}

/// covers: PROC-24
#[test]
fn era_shims_treat_chunk_and_expanded_forms_alike() {
    let sink = Arc::new(CollectingSink::new());
    let mut reporter = Reporter::new(sink.clone(), 64);
    let shims = EraShims::new(LegacyEra::V0_0_57);
    let chunk_form = in_run(&[
        json!({"type": "TEXT_MESSAGE_CHUNK", "messageId": "m1", "delta": "a", "subagentRunId": "s1"}),
    ]);
    let expanded_form: Vec<Value> = expand(&chunk_form)
        .unwrap()
        .iter()
        .map(|e| serde_json::to_value(e).unwrap())
        .collect();
    for form in [chunk_form, expanded_form] {
        let mut out = Vec::new();
        for mut frame in form {
            if shims.upgrade_event(&mut frame, &mut reporter) {
                out.push(frame);
            }
        }
        let text = serde_json::to_string(&out).unwrap();
        assert!(!text.contains("subagentRunId"), "{text}");
        let r = run(&out);
        r.assert_ok();
        assert_eq!(r.message("m1")["content"], "a");
    }
    assert!(sink.take().is_empty(), "stripping attribution is silent");
}

/// covers: LIFE-16
#[cfg(feature = "client")]
#[test]
fn a_cancelled_run_leaves_nothing_to_resume() {
    use agui_sdk::client::{AgentClient, RunRequest};
    use agui_sdk::diagnostics::NullSink;
    use agui_sdk::interrupt::ResumeEntry;
    use agui_sdk::ErrorCode;
    use support_client::{block_on, mock, run_finished, run_started, text};

    let mut frames = vec![run_started("t1", "r1")];
    frames.extend(text("m1", "partial"));
    let mut terminal = run_finished("t1", "r1");
    terminal["outcome"] = json!({"type": "cancelled"});
    frames.push(terminal);
    let transport = mock(frames);
    let client = AgentClient::with_transport(transport.clone())
        .diagnostic_sink(NullSink)
        .build()
        .unwrap();
    let mut thread = client.thread_with_id("t1");
    let report =
        block_on(thread.run_and_absorb(RunRequest::new().run_id("r1").user("go"))).unwrap();
    assert!(report.status.is_cancelled());
    assert!(thread.open_interrupts().is_empty());
    let err = thread
        .resume(vec![ResumeEntry::resolved("i1", json!(true))])
        .unwrap_err();
    assert_eq!(err.code(), ErrorCode::ResumeCoverage);
    assert_eq!(transport.requests().len(), 1, "nothing was sent for it");
}
