//! Runs and steps, consumer side (`events/lifecycle.mdx`).

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[cfg(feature = "client")]
mod support_client;
mod support_pipeline;

use agui_sdk::diagnostics::DiagnosticCode;
use agui_sdk::event::Event;
use agui_sdk::lifecycle::RunFinishedOutcome;
use agui_sdk::pipeline::verify::RunPhase;
use agui_sdk::pipeline::Termination;
use agui_sdk::{ErrorCode, ErrorKind};
use serde_json::json;
use support_pipeline::{errored, finished, in_run, run, started, text, verify};

fn outcome(r: &support_pipeline::Run) -> Option<RunFinishedOutcome> {
    r.events.iter().find_map(|e| match e {
        Event::RunFinished(f) => Some(f.outcome.clone()),
        _ => None,
    })?
}

/// What `AgentClient` reports for `stream` over the in-memory transport:
/// the status the application reads.
#[cfg(feature = "client")]
fn client_status(stream: &[serde_json::Value]) -> agui_sdk::client::RunStatus {
    client_report(stream).status
}

/// The whole report `AgentClient` returns for `stream`.
#[cfg(feature = "client")]
fn client_report(stream: &[serde_json::Value]) -> agui_sdk::client::RunReport {
    use std::future::IntoFuture;
    let client =
        agui_sdk::client::AgentClient::with_transport(support_client::mock(stream.to_vec()))
            .diagnostic_sink(agui_sdk::diagnostics::NullSink)
            .build()
            .unwrap();
    let request = agui_sdk::client::RunRequest::new()
        .thread_id("t")
        .run_id("r1")
        .user("hi");
    support_client::block_on(client.run(request).into_future()).unwrap()
}

/// covers: LIFE-33, LIFE-39
#[cfg(feature = "client")]
#[test]
fn a_late_run_error_without_usage_keeps_the_usage_run_finished_reported() {
    let usage = json!([{"provider": "p", "model": "m",
                        "inputTokens": 10, "outputTokens": 5, "totalTokens": 15}]);
    let mut done = finished("r1");
    done["usage"] = usage.clone();
    let report = client_report(&[started("r1"), done.clone(), errored("flush failed")]);
    assert!(!report.status.is_success());
    assert_eq!(serde_json::to_value(&report.usage).unwrap(), usage);

    // A late error that reports usage of its own is the run's latest word.
    let mut late = errored("flush failed");
    late["usage"] = json!([{"provider": "p", "model": "m", "totalTokens": 20}]);
    let report = client_report(&[started("r1"), done, late.clone()]);
    assert_eq!(serde_json::to_value(&report.usage).unwrap(), late["usage"]);

    // A run that fails before finishing reports what its error carries.
    assert!(client_report(&[started("r1"), errored("x")])
        .usage
        .is_empty());
}

/// covers: LIFE-03
#[test]
fn a_stream_opening_with_anything_else_is_a_violation() {
    let r = run(&[json!({"type": "TEXT_MESSAGE_START", "messageId": "m1", "role": "assistant"})]);
    r.assert_rejected("First event must be 'RUN_STARTED'");
    assert_eq!(
        r.error.as_ref().map(|e| e.code()),
        Some(ErrorCode::RunLifecycle)
    );
    assert!(r.events.is_empty());

    // RUN_ERROR is admitted first.
    let r = run(&[errored("unreachable agent")]);
    r.assert_ok();
    assert_eq!(r.types(), ["RUN_ERROR"]);
}

/// covers: LIFE-05
#[test]
fn run_finished_outcomes_are_delivered_as_sent() {
    let cases = [
        (json!(null), None),
        (json!({"type": "success"}), Some("success")),
        (
            json!({"type": "interrupt", "interrupts": [{"id": "i1", "reason": "approval"}]}),
            Some("interrupt"),
        ),
        (json!({"type": "cancelled"}), Some("cancelled")),
    ];
    for (sent, kind) in cases {
        let mut terminal = finished("r1");
        if !sent.is_null() {
            terminal["outcome"] = sent.clone();
        }
        let r = run(&[started("r1"), terminal]);
        r.assert_ok();
        let wire = r.wire();
        match kind {
            None => assert!(wire[1].get("outcome").is_none()),
            Some(kind) => assert_eq!(wire[1]["outcome"]["type"], kind),
        }
        if kind == Some("interrupt") {
            assert_eq!(wire[1]["outcome"]["interrupts"][0]["id"], "i1");
        }
    }
}

/// covers: LIFE-06
#[test]
fn success_outcome_names_unanswered_frontend_calls() {
    let mut terminal = finished("r1");
    terminal["outcome"] = json!({"type": "success", "pendingToolCallIds": ["t1"]});
    let r = run(&[
        started("r1"),
        json!({"type": "TOOL_CALL_START", "toolCallId": "t1", "toolCallName": "confirm"}),
        json!({"type": "TOOL_CALL_ARGS", "toolCallId": "t1", "delta": "{}"}),
        json!({"type": "TOOL_CALL_END", "toolCallId": "t1"}),
        terminal,
    ]);
    r.assert_ok();
    assert_eq!(r.wire()[4]["outcome"]["pendingToolCallIds"], json!(["t1"]));
}

/// covers: LIFE-08
#[test]
fn result_and_usage_are_delivered() {
    let mut terminal = finished("r1");
    terminal["result"] = json!({"answer": 42});
    terminal["usage"] = json!([
        {"provider": "p", "model": "a", "inputTokens": 10, "outputTokens": 2},
        {"provider": "p", "model": "b", "inputTokens": 1, "outputTokens": 1}
    ]);
    let r = run(&[started("r1"), terminal]);
    r.assert_ok();
    let wire = r.wire();
    assert_eq!(wire[1]["result"], json!({"answer": 42}));
    assert_eq!(wire[1]["usage"].as_array().unwrap().len(), 2);
}

/// covers: LIFE-09
#[test]
fn an_unrecognised_outcome_is_stripped_and_reads_as_success() {
    let mut terminal = finished("r1");
    terminal["outcome"] = json!({"type": "paused", "until": "later"});
    let r = run(&[started("r1"), terminal]);
    r.assert_ok();
    assert!(r.wire()[1].get("outcome").is_none());
    assert!(r.warned(DiagnosticCode::UnknownVariant));
    assert_eq!(outcome(&r), None);
}

/// covers: LIFE-16, LIFE-17, LIFE-18
#[test]
fn a_cancelled_run_is_neither_success_nor_failure_and_the_next_run_is_ordinary() {
    let mut terminal = finished("r1");
    terminal["outcome"] = json!({"type": "cancelled"});
    let mut stream = vec![started("r1")];
    stream.extend(text("m1", "partial"));
    stream.push(terminal.clone());
    let r = run(&stream);
    r.assert_ok();
    assert_eq!(outcome(&r), Some(RunFinishedOutcome::Cancelled));
    assert!(r.error.is_none(), "a stop is not an error");
    assert_eq!(verify(&stream).unwrap(), RunPhase::Finished);
    // What the application is told: cancelled, neither success nor failure.
    #[cfg(feature = "client")]
    {
        let status = client_status(&stream);
        assert!(status.is_cancelled(), "{status:?}");
        assert!(!status.is_success(), "a cancelled run read as success");
        assert!(
            !matches!(status, agui_sdk::client::RunStatus::Errored(_)),
            "a cancelled run read as failure"
        );
    }

    // The next run is an ordinary new run.
    stream.push(started("r2"));
    stream.push(finished("r2"));
    run(&stream).assert_ok();
}

/// covers: LIFE-19
#[test]
fn everything_delivered_before_a_failure_stays_delivered() {
    let mut stream = vec![started("r1")];
    stream.extend(text("m1", "kept"));
    stream.push(json!({"type": "STATE_SNAPSHOT", "snapshot": {"k": 1}}));
    stream.push(errored("boom"));
    let r = run(&stream);
    r.assert_ok();
    assert_eq!(r.message("m1")["content"], "kept");
    assert_eq!(r.state, json!({"k": 1}));

    // The same for a stream the pipeline rejects.
    let mut stream = vec![started("r1")];
    stream.extend(text("m1", "kept"));
    stream.push(json!({"type": "TEXT_MESSAGE_END", "messageId": "m1"}));
    let r = run(&stream);
    assert!(r.error.is_some());
    assert_eq!(r.message("m1")["content"], "kept");
}

/// covers: LIFE-20
#[test]
fn a_truncated_stream_gets_no_synthesized_terminal() {
    let r = run(&[
        started("r1"),
        json!({"type": "TEXT_MESSAGE_CHUNK", "messageId": "m1", "delta": "cut"}),
    ]);
    assert!(r.error.is_none());
    assert_eq!(r.termination, Termination::Truncated);
    assert!(!r.types().contains(&"RUN_FINISHED"));
    assert!(!r.types().contains(&"TEXT_MESSAGE_END"));

    assert_eq!(run(&[]).termination, Termination::Truncated);
    assert_eq!(
        run(&[started("r1"), finished("r1")]).termination,
        Termination::Complete
    );
}

/// covers: LIFE-21, LIFE-22, LIFE-23
#[test]
fn run_error_is_a_well_formed_event_delivered_to_the_application() {
    let r = run(&[
        started("r1"),
        json!({"type": "RUN_ERROR", "message": "rate limited", "code": "RATE_LIMIT",
               "usage": [{"provider": "p", "model": "m", "inputTokens": 3}]}),
    ]);
    r.assert_ok();
    let wire = r.wire();
    assert_eq!(wire[1]["type"], "RUN_ERROR");
    assert_eq!(wire[1]["message"], "rate limited");
    assert_eq!(wire[1]["code"], "RATE_LIMIT");
    assert_eq!(wire[1]["usage"][0]["inputTokens"], 3);
}

/// covers: LIFE-24, LIFE-40
#[test]
fn a_run_ending_in_run_error_is_failed_even_after_run_finished() {
    assert_eq!(
        verify(&[started("r1"), errored("x")]).unwrap(),
        RunPhase::Failed
    );
    let late = [started("r1"), finished("r1"), errored("flush failed")];
    assert_eq!(verify(&late).unwrap(), RunPhase::Failed);
    let r = run(&late);
    r.assert_ok();
    assert_eq!(r.types(), ["RUN_STARTED", "RUN_FINISHED", "RUN_ERROR"]);
    // What the application is told: failed, both when the error is the only
    // terminal and when it follows RUN_FINISHED.
    #[cfg(feature = "client")]
    for stream in [&[started("r1"), errored("x")][..], &late[..]] {
        let status = client_status(stream);
        assert!(!status.is_success(), "{status:?}");
        assert!(
            matches!(status, agui_sdk::client::RunStatus::Errored(_)),
            "{status:?}"
        );
    }
    // A late error fails an interrupted run too: it leaves nothing awaiting
    // answers.
    #[cfg(feature = "client")]
    {
        let interrupted = json!({"type": "RUN_FINISHED", "threadId": "t", "runId": "r1",
            "outcome": {"type": "interrupt", "interrupts": [{"id": "i", "reason": "x"}]}});
        let report = client_report(&[started("r1"), interrupted, errored("late")]);
        assert!(
            matches!(report.status, agui_sdk::client::RunStatus::Errored(_)),
            "{:?}",
            report.status
        );
        assert!(report.interrupts.is_empty(), "{:?}", report.interrupts);
    }
}

/// covers: LIFE-38, LIFE-39
#[test]
fn a_new_run_may_follow_a_closed_one_and_a_late_error_may_follow_success() {
    run(&[started("r1"), finished("r1"), started("r2"), finished("r2")]).assert_ok();
    run(&[started("r1"), errored("x"), started("r2"), finished("r2")]).assert_ok();
    run(&[errored("x"), started("r2"), finished("r2")]).assert_ok();
    run(&[
        started("r1"),
        finished("r1"),
        errored("late"),
        started("r2"),
        finished("r2"),
    ])
    .assert_ok();
}

/// covers: LIFE-42
#[test]
fn any_other_event_after_close_is_rejected() {
    let r = run(&[
        started("r1"),
        finished("r1"),
        json!({"type": "TEXT_MESSAGE_START", "messageId": "m1", "role": "assistant"}),
    ]);
    r.assert_rejected("The run has already finished with 'RUN_FINISHED'");
    let r = run(&[
        started("r1"),
        errored("x"),
        json!({"type": "CUSTOM", "name": "n", "value": 1}),
    ]);
    r.assert_rejected("The run has already errored with 'RUN_ERROR'");
    let r = run(&[started("r1"), errored("x"), errored("again")]);
    r.assert_rejected("The run has already errored with 'RUN_ERROR'");
    let r = run(&[started("r1"), finished("r1"), finished("r1")]);
    r.assert_rejected("The run has already finished with 'RUN_FINISHED'");
}

/// covers: LIFE-43, LIFE-45
#[test]
fn several_runs_accumulate_messages_and_keep_state() {
    let mut stream = vec![started("r1")];
    stream.extend(text("m1", "first"));
    stream.push(json!({"type": "STATE_SNAPSHOT", "snapshot": {"n": 1}}));
    stream.push(finished("r1"));
    stream.push(started("r2"));
    stream.extend(text("m2", "second"));
    stream.push(json!({"type": "STATE_DELTA", "delta": [{"op": "add", "path": "/m", "value": 2}]}));
    stream.push(finished("r2"));
    let r = run(&stream);
    r.assert_ok();
    assert_eq!(r.messages.len(), 2);
    assert_eq!(r.message("m1")["content"], "first");
    assert_eq!(r.message("m2")["content"], "second");
    assert_eq!(r.state, json!({"n": 1, "m": 2}));
}

/// covers: LIFE-47
#[test]
fn re_streaming_a_held_message_appends_to_it() {
    let mut stream = vec![started("r1")];
    stream.extend(text("m1", "First half."));
    stream.extend(text("m1", " Second half."));
    stream.push(finished("r1"));
    stream.push(started("r2"));
    stream.extend(text("m1", " Third."));
    stream.push(finished("r2"));
    let r = run(&stream);
    r.assert_ok();
    assert_eq!(r.messages.len(), 1);
    assert_eq!(
        r.message("m1")["content"],
        "First half. Second half. Third."
    );
}

/// covers: LIFE-48
#[test]
fn run_scoped_tracking_resets_and_run_error_ends_what_was_open() {
    let r = run(&[
        started("r1"),
        json!({"type": "TEXT_MESSAGE_START", "messageId": "m1"}),
        json!({"type": "TOOL_CALL_START", "toolCallId": "t1", "toolCallName": "f"}),
        json!({"type": "STEP_STARTED", "stepName": "s"}),
        json!({"type": "SUBAGENT_STARTED", "subagentRunId": "sa", "name": "n"}),
        errored("boom"),
        started("r2"),
        // Nothing carried over: the same ids open afresh...
        json!({"type": "STEP_STARTED", "stepName": "s"}),
        json!({"type": "SUBAGENT_STARTED", "subagentRunId": "sb", "name": "n"}),
        json!({"type": "SUBAGENT_FINISHED", "subagentRunId": "sb"}),
        json!({"type": "STEP_FINISHED", "stepName": "s"}),
        finished("r2"),
    ]);
    r.assert_ok();
    // ...and nothing open in the failed run continues into the next.
    run(&[
        started("r1"),
        json!({"type": "TEXT_MESSAGE_START", "messageId": "m1"}),
        errored("boom"),
        started("r2"),
        json!({"type": "TEXT_MESSAGE_CONTENT", "messageId": "m1", "delta": "x"}),
    ])
    .assert_rejected("No active text message found with ID 'm1'");
}

/// covers: LIFE-52
#[test]
fn steps_overlap_each_other_and_everything_else() {
    run(&in_run(&[
        json!({"type": "STEP_STARTED", "stepName": "a"}),
        json!({"type": "TEXT_MESSAGE_START", "messageId": "m1"}),
        json!({"type": "STEP_STARTED", "stepName": "b"}),
        json!({"type": "STEP_FINISHED", "stepName": "a"}),
        json!({"type": "TEXT_MESSAGE_END", "messageId": "m1"}),
        json!({"type": "STEP_FINISHED", "stepName": "b"}),
        json!({"type": "STEP_STARTED", "stepName": "a"}),
        json!({"type": "STEP_FINISHED", "stepName": "a"}),
    ]))
    .assert_ok();
}

/// covers: LIFE-53
#[test]
fn every_lifecycle_violation_is_fatal() {
    run(&[
        started("r1"),
        json!({"type": "RUN_STARTED", "threadId": "t", "runId": "r2"}),
    ])
    .assert_rejected("Cannot send 'RUN_STARTED' while a run is still active");
    let r = run(&in_run(&[
        json!({"type": "STEP_FINISHED", "stepName": "never-opened"}),
    ]));
    r.assert_rejected("that was not started");
    assert_eq!(r.error.as_ref().map(|e| e.code()), Some(ErrorCode::Step));
    run(&in_run(&[
        json!({"type": "STEP_STARTED", "stepName": "s"}),
        json!({"type": "STEP_STARTED", "stepName": "s"}),
    ]))
    .assert_rejected("Step 's' is already active");
    run(&[
        started("r1"),
        json!({"type": "STEP_STARTED", "stepName": "s"}),
        finished("r1"),
    ])
    .assert_rejected("Cannot send 'RUN_FINISHED' while steps are still active: 's'");
    run(&[
        started("r1"),
        json!({"type": "TEXT_MESSAGE_START", "messageId": "m1"}),
        json!({"type": "TEXT_MESSAGE_CONTENT", "messageId": "m1", "delta": "never closed"}),
        finished("r1"),
    ])
    .assert_rejected("Cannot send 'RUN_FINISHED' while text messages are still active");
    run(&[
        started("r1"),
        json!({"type": "TOOL_CALL_START", "toolCallId": "t1", "toolCallName": "f"}),
        finished("r1"),
    ])
    .assert_rejected("Cannot send 'RUN_FINISHED' while tool calls are still active: 't1'");
    let r = run(&[
        started("r1"),
        json!({"type": "RUN_FINISHED", "threadId": "t", "runId": "other"}),
    ]);
    assert_eq!(
        r.error.as_ref().map(|e| e.code()),
        Some(ErrorCode::Identity)
    );
}

/// covers: LIFE-54
#[test]
fn a_rejected_stream_and_a_reported_failure_are_different_things() {
    let reported = run(&[started("r1"), errored("model failed")]);
    assert!(reported.error.is_none());
    assert_eq!(reported.termination, Termination::Complete);
    assert_eq!(reported.types().last(), Some(&"RUN_ERROR"));

    let rejected = run(&[
        started("r1"),
        json!({"type": "TEXT_MESSAGE_END", "messageId": "x"}),
    ]);
    let err = rejected.error.as_ref().unwrap();
    assert_eq!(err.kind(), ErrorKind::Protocol);

    // A rejected stream stays rejected: the processor refuses to continue it.
    use agui_sdk::diagnostics::{NullSink, Reporter};
    use agui_sdk::pipeline::{PipelineConfig, Processor};
    let mut processor = Processor::new(PipelineConfig::default());
    let mut reporter = Reporter::new(std::sync::Arc::new(NullSink), 16);
    let mut out = Vec::new();
    for frame in [
        started("r1"),
        json!({"type": "TEXT_MESSAGE_END", "messageId": "x"}),
    ] {
        let _ = processor.push_json(
            &serde_json::to_vec(&frame).unwrap(),
            &mut reporter,
            &mut out,
        );
    }
    let again = processor
        .push_json(
            &serde_json::to_vec(&finished("r1")).unwrap(),
            &mut reporter,
            &mut out,
        )
        .unwrap_err();
    assert_eq!(again.code(), ErrorCode::TextMessage);
    assert!(!rejected.types().contains(&"RUN_ERROR"));
}

/// covers: LIFE-43
#[test]
fn a_processor_expecting_a_run_accepts_replayed_history_first() {
    use agui_sdk::ids::{RunId, ThreadId};
    use agui_sdk::pipeline::{PipelineConfig, Processor};
    let expecting = || {
        Processor::new(PipelineConfig::default()).expecting(ThreadId::new("t"), RunId::new("r2"))
    };
    let mut stream = vec![started("r1")];
    stream.extend(text("m1", "replayed"));
    stream.extend([finished("r1"), started("r2"), finished("r2")]);
    support_pipeline::drive_with(expecting(), &stream, false).assert_ok();

    // A run after the requested one is accepted too: the requested run need
    // not be the last on the stream.
    stream.extend([started("r3"), finished("r3")]);
    support_pipeline::drive_with(expecting(), &stream, false).assert_ok();

    // But the requested run must appear: replayed history alone is not it.
    let only_others = [started("r1"), finished("r1"), started("r3"), finished("r3")];
    let r = support_pipeline::drive_with(expecting(), &only_others, false);
    assert_eq!(
        r.error.as_ref().map(|e| e.code()),
        Some(ErrorCode::Identity)
    );
}
