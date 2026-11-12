//! Server-side run lifecycle: what the runner emits around an agent, how it
//! closes a run on every path, and the exactly-one-terminal guarantee.

#![cfg(feature = "server")]
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support_pipeline;
mod support_server;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use agui_sdk::event::Event;
use agui_sdk::ids::{RunId, ToolCallId};
use agui_sdk::interrupt::Interrupt;
use agui_sdk::lifecycle::{
    RunError, RunFinished, RunFinishedOutcome, RunStarted, StepFinished, StepStarted,
};
use agui_sdk::pipeline::verify::{ProducerRule, ProducerVerifier};
use agui_sdk::server::emit::Emitter;
use agui_sdk::server::helpers::InterruptStore;
use agui_sdk::server::{
    CancelToken, Extensions, ReplayedRun, RunOutcome, Runner, RunnerConfig, Transformer,
};
use agui_sdk::text::{TextMessageChunk, TextMessageRole};
use agui_sdk::usage::TokenUsage;
use agui_sdk::{Error, ErrorCode, EventType, Message};
use serde_json::json;

use support_server::*;

/// covers: EVT-01, LIFE-01, LIFE-04, LIFE-05
#[test]
fn a_quiet_agent_gets_exactly_the_lifecycle() {
    let runner = Runner::new(agent(|_ctx| async { Ok(RunOutcome::success()) }));
    let events = run(&runner, input().with_parent_run_id("r0"));
    assert_eq!(
        types(&events),
        [EventType::RunStarted, EventType::RunFinished]
    );
    let Event::RunStarted(started) = &events[0] else {
        panic!("first event")
    };
    assert_eq!(started.thread_id.as_str(), "t1");
    assert_eq!(started.run_id.as_str(), "r1");
    assert_eq!(started.protocol_version.as_deref(), Some("1.0"));
    assert_eq!(
        started.parent_run_id.as_ref().map(RunId::as_str),
        Some("r0")
    );
    assert!(started.input.is_none(), "input echo is off by default");
    let finished = finished(&events);
    assert_eq!(finished.outcome, Some(RunFinishedOutcome::success()));
    assert!(finished.result.is_none() && finished.usage.is_none());
}

/// covers: LIFE-02, LIFE-37, LIFE-41, LIFE-44
#[test]
fn agents_cannot_emit_lifecycle_events() {
    let lifecycle: Vec<fn() -> Event> = vec![
        || RunStarted::new("t1", "r2").into(),
        || RunFinished::new("t1", "r1").into(),
        || RunError::new("boom").into(),
    ];
    for make in lifecycle {
        let seen = Arc::new(Mutex::new(None));
        let seen2 = Arc::clone(&seen);
        let runner = Runner::new(agent(move |ctx| {
            let seen = Arc::clone(&seen2);
            async move {
                let result = ctx.emitter().emit(make()).await;
                *seen.lock().unwrap() = result.err().map(|e| e.code());
                Ok(RunOutcome::success())
            }
        }));
        let events = run(&runner, input());
        assert_eq!(*seen.lock().unwrap(), Some(ErrorCode::ProducerViolation));
        assert_eq!(types(&events), [EventType::RunStarted, EventType::RunError]);
        assert_eq!(
            run_error(&events).code.as_deref(),
            Some("server.producer_violation")
        );
        assert_one_valid_run(&events);
    }
}

/// covers: LIFE-06, CHG-03
#[test]
fn success_names_unanswered_frontend_calls() {
    let runner = Runner::new(agent(|ctx| async move {
        let call = ctx.emitter().start_tool_call("render_chart", None).await?;
        call.push_args("{}").await?;
        call.end().await?;
        Ok(RunOutcome::success())
    }));
    let input = input().with_tools([agui_sdk::tool::Tool::new("render_chart", "draws")]);
    let events = run(&runner, input);
    let Event::ToolCallStart(start) = &events[1] else {
        panic!("tool call start")
    };
    let finished = finished(&events);
    assert_eq!(
        finished.outcome,
        Some(RunFinishedOutcome::success_with_pending([start
            .tool_call_id
            .clone()]))
    );
}

/// covers: LIFE-07
#[test]
fn outcomes_are_only_schema_values() {
    let outcomes = [
        RunOutcome::success(),
        RunOutcome::interrupt([Interrupt::new("i1", "approval")]),
        RunOutcome::cancelled(),
    ];
    for outcome in outcomes {
        let runner = Runner::new(agent(move |_ctx| {
            let outcome = outcome.clone();
            async move { Ok(outcome) }
        }));
        let events = run(&runner, input());
        let wire = serde_json::to_value(terminal(&events)).unwrap();
        let kind = wire["outcome"]["type"].as_str().unwrap_or("").to_owned();
        assert!(
            ["success", "interrupt", "cancelled"].contains(&kind.as_str()),
            "outcome {kind}"
        );
    }
}

fn usage_of(provider: &str, input: u64, output: u64) -> TokenUsage {
    TokenUsage::default()
        .with_provider(provider)
        .with_model("m")
        .with_input_tokens(input)
        .with_output_tokens(output)
        .with_total_tokens(input + output)
}

/// covers: LIFE-08, CHG-02
#[test]
fn finish_carries_result_and_one_usage_entry_per_model() {
    let runner = Runner::new(agent(|ctx| async move {
        ctx.emitter().record_usage(usage_of("anthropic", 10, 5));
        ctx.emitter().record_usage(usage_of("anthropic", 20, 1));
        ctx.emitter().record_usage(usage_of("openai", 1, 1));
        Ok(RunOutcome::success().with_result(json!({"answer": 42})))
    }));
    let events = run(&runner, input());
    let finished = finished(&events);
    assert_eq!(finished.result, Some(json!({"answer": 42})));
    let usage = finished.usage.clone().unwrap_or_default();
    assert_eq!(usage.len(), 2);
    assert_eq!(usage[0].input_tokens, Some(30));
    assert_eq!(usage[0].output_tokens, Some(6));
    assert_eq!(usage[0].total_tokens, Some(36));

    // RUN_ERROR reports usage in the same accounting.
    let runner = Runner::new(agent(|ctx| async move {
        ctx.emitter().record_usage(usage_of("anthropic", 3, 4));
        Err(Error::new(ErrorCode::AgentFailed, "model unavailable").into())
    }));
    let events = run(&runner, input());
    let error = run_error(&events);
    assert_eq!(error.usage.as_ref().map(Vec::len), Some(1));
}

async fn open_everything(ctx: &agui_sdk::server::RunContext) -> agui_sdk::Result<()> {
    let e = ctx.emitter();
    let _step = e.start_step("plan").await?;
    let text = e.start_text(TextMessageRole::Assistant).await?;
    text.push("partial").await?;
    let call = e.start_tool_call("search", None).await?;
    call.push_args("{\"q\":").await?;
    let span = e.start_reasoning().await?;
    let _thought = span.start_message().await?;
    let sub = e.start_subagent("helper").await?;
    let sub_text = sub.emitter().start_text(TextMessageRole::Assistant).await?;
    sub_text.push("x").await?;
    std::mem::forget((text, call, span, sub, sub_text));
    Ok(())
}

/// covers: LIFE-10, LIFE-11, LIFE-12, LIFE-15, LIFE-16
#[test]
fn cancellation_closes_everything_then_finishes_cancelled() {
    let runner = Runner::new(agent(|ctx| async move {
        ctx.emitter().record_usage(usage_of("anthropic", 7, 2));
        open_everything(&ctx).await?;
        ctx.cancellation().cancel();
        // A cancelled run's sends fail; the agent may also just stop.
        let after = ctx.emitter().say("too late").await;
        assert_eq!(after.err().map(|e| e.code()), Some(ErrorCode::Cancelled));
        std::future::pending::<()>().await;
        Ok(RunOutcome::success())
    }));
    let events = run(&runner, input());
    assert_one_valid_run(&events);
    let finished = finished(&events);
    assert_eq!(finished.outcome, Some(RunFinishedOutcome::cancelled()));
    assert!(finished.result.is_none());
    assert_eq!(finished.usage.as_ref().map(Vec::len), Some(1));
    // Every opener has its closer before the terminal.
    let closers = [
        EventType::ReasoningMessageEnd,
        EventType::ReasoningEnd,
        EventType::TextMessageEnd,
        EventType::ToolCallEnd,
        EventType::StepFinished,
        EventType::SubagentError,
    ];
    for kind in closers {
        assert!(types(&events).contains(&kind), "missing {kind:?}");
    }
}

/// The wire form of a runner's events, for the client-side pipeline.
fn wire(events: &[Event]) -> Vec<serde_json::Value> {
    events
        .iter()
        .map(|e| serde_json::to_value(e).unwrap())
        .collect()
}

/// covers: LIFE-12, SUB-14
#[test]
fn a_cancelled_run_fails_the_invocations_it_cut_off() {
    let runner = Runner::new(agent(|ctx| async move {
        let p = ctx.emitter().start_subagent("p").await?;
        let c = p.start_subagent("c").await?;
        std::mem::forget((p, c));
        ctx.cancellation().cancel();
        std::future::pending::<()>().await;
        Ok(RunOutcome::success())
    }));
    let events = run(&runner, input());
    assert_one_valid_run(&events);
    use EventType::*;
    let kinds = types(&events);
    let first = kinds.iter().position(|t| *t == SubagentStarted).unwrap();
    assert_eq!(
        kinds[first..],
        [
            SubagentStarted,
            SubagentStarted,
            SubagentError,
            SubagentError,
            RunFinished
        ]
    );
    let started: Vec<_> = events
        .iter()
        .filter_map(|e| match e {
            Event::SubagentStarted(s) => Some(s.subagent_run_id.clone()),
            _ => None,
        })
        .collect();
    let errors: Vec<_> = events
        .iter()
        .filter_map(|e| match e {
            Event::SubagentError(s) => Some((s.subagent_run_id.clone(), s.message.clone())),
            _ => None,
        })
        .collect();
    // Innermost first.
    assert_eq!(errors[0].0, started[1]);
    assert_eq!(errors[1].0, started[0]);
    assert!(errors.iter().all(|(_, m)| m == "cancelled with the run"));
    assert_eq!(
        finished(&events).outcome,
        Some(RunFinishedOutcome::cancelled())
    );
    // The independent producer verifier accepts the stream.
    let mut verifier = ProducerVerifier::new();
    for event in &events {
        verifier.check(event).unwrap();
    }

    // The client delivers the invocations as failed, the run as cancelled.
    let client = support_pipeline::run(&wire(&events));
    client.assert_ok();
    assert_eq!(
        client
            .events
            .iter()
            .filter(
                |e| matches!(e, Event::SubagentError(e) if e.message == "cancelled with the run")
            )
            .count(),
        2
    );
    assert!(!client
        .events
        .iter()
        .any(|e| matches!(e, Event::SubagentFinished(_))));
    assert_eq!(
        client.termination,
        agui_sdk::pipeline::Termination::Complete
    );
}

/// covers: SUB-16, SUB-35, INTR-06
#[test]
fn an_interrupted_run_suspends_the_invocations_still_open() {
    let runner = Runner::new(agent(|ctx| async move {
        let p = ctx.emitter().start_subagent("p").await?;
        let c = p.start_subagent("c").await?;
        let c_id = c.id().clone();
        std::mem::forget((p, c));
        Ok(RunOutcome::interrupt([
            Interrupt::new("i-child", "approval").with_subagent_run_id(c_id),
            Interrupt::new("i-parent", "approval"),
        ]))
    }));
    let events = run(&runner, input());
    assert_one_valid_run(&events);
    use agui_sdk::subagent::SubagentFinishedOutcome;
    let started: Vec<_> = events
        .iter()
        .filter_map(|e| match e {
            Event::SubagentStarted(s) => Some(s.subagent_run_id.clone()),
            _ => None,
        })
        .collect();
    let closed: Vec<_> = events
        .iter()
        .filter_map(|e| match e {
            Event::SubagentFinished(f) => Some((f.subagent_run_id.clone(), f.outcome.clone())),
            Event::SubagentError(f) => panic!("unexpected SUBAGENT_ERROR {f:?}"),
            _ => None,
        })
        .collect();
    // Innermost first; each names only the interrupts it raised itself.
    assert_eq!(closed.len(), 2);
    assert_eq!(closed[0].0, started[1]);
    assert_eq!(
        closed[0].1,
        Some(SubagentFinishedOutcome::suspended([
            agui_sdk::ids::InterruptId::from("i-child")
        ]))
    );
    assert_eq!(closed[1].0, started[0]);
    assert!(matches!(
        &closed[1].1,
        Some(SubagentFinishedOutcome::Suspended {
            interrupt_ids: None,
            ..
        })
    ));
    assert!(matches!(
        finished(&events).outcome,
        Some(RunFinishedOutcome::Interrupt { .. })
    ));
    let mut verifier = ProducerVerifier::new();
    for event in &events {
        verifier.check(event).unwrap();
    }

    let client = support_pipeline::run(&wire(&events));
    client.assert_ok();
    let suspended = client
        .events
        .iter()
        .filter(|e| {
            matches!(e, Event::SubagentFinished(f)
                if matches!(f.outcome, Some(SubagentFinishedOutcome::Suspended { .. })))
        })
        .count();
    assert_eq!(suspended, 2);
}

/// covers: SUB-16
#[test]
fn a_successful_run_still_finishes_the_invocations_left_open() {
    let runner = Runner::new(agent(|ctx| async move {
        let p = ctx.emitter().start_subagent("p").await?;
        std::mem::forget(p);
        Ok(RunOutcome::success())
    }));
    let events = run(&runner, input());
    assert_one_valid_run(&events);
    let closers: Vec<_> = events
        .iter()
        .filter(|e| matches!(e, Event::SubagentFinished(_) | Event::SubagentError(_)))
        .collect();
    assert!(matches!(
        closers[..],
        [Event::SubagentFinished(f)] if f.outcome.is_none()
    ));
}

/// covers: LIFE-14
#[test]
fn a_cancelled_outcome_has_no_result() {
    assert_eq!(
        RunOutcome::cancelled().with_result(json!(1)),
        RunOutcome::cancelled()
    );
    assert_eq!(
        RunOutcome::interrupt([Interrupt::new("i", "r")]).with_result(json!(1)),
        RunOutcome::interrupt([Interrupt::new("i", "r")])
    );
}

#[test]
fn a_refused_send_ends_the_run_even_if_the_agent_carries_on() {
    let runner = Runner::new(agent(|ctx| async move {
        let _ = ctx.emitter().emit(StepFinished::new("never-opened")).await;
        // An agent that ignores the error and never returns.
        std::future::pending::<()>().await;
        Ok(RunOutcome::success())
    }));
    let events = run(&runner, input());
    assert_eq!(types(&events), [EventType::RunStarted, EventType::RunError]);
}

#[test]
fn a_finishing_subagent_closes_what_it_left_open() {
    let runner = Runner::new(agent(|ctx| async move {
        let sub = ctx.emitter().start_subagent("helper").await?;
        let text = sub.emitter().start_text(TextMessageRole::Assistant).await?;
        text.push("half").await?;
        let nested = sub.start_subagent("inner").await?;
        // Left open on purpose: the closers finish it (D12).
        let _ = nested.emitter().start_step("s").await?;
        std::mem::forget((text, nested));
        let outer = ctx.emitter().start_text(TextMessageRole::Assistant).await?;
        sub.finish(None, None).await?;
        outer.end().await?;
        Ok(RunOutcome::success())
    }));
    let events = run(&runner, input());
    assert_one_valid_run(&events);
    assert_eq!(terminal(&events).event_type(), EventType::RunFinished);
    let kinds = types(&events);
    let finished_at = kinds
        .iter()
        .rposition(|t| *t == EventType::SubagentFinished)
        .unwrap();
    // The parent's own message is not closed by the subagent's finish.
    let outer_end = kinds
        .iter()
        .rposition(|t| *t == EventType::TextMessageEnd)
        .unwrap();
    assert!(outer_end > finished_at);
    assert_eq!(
        kinds
            .iter()
            .filter(|t| **t == EventType::SubagentFinished)
            .count(),
        2
    );
}

/// covers: SUB-14
#[test]
fn a_failed_invocation_fails_its_still_active_descendants() {
    let runner = Runner::new(agent(|ctx| async move {
        let p = ctx.emitter().start_subagent("p").await?;
        let c = p.start_subagent("c").await?;
        std::mem::forget(c);
        p.fail("boom").await?;
        Ok(RunOutcome::success())
    }));
    let events = run(&runner, input());
    assert_one_valid_run(&events);
    use EventType::*;
    let kinds = types(&events);
    let first = kinds.iter().position(|t| *t == SubagentStarted).unwrap();
    assert_eq!(
        kinds[first..],
        [
            SubagentStarted,
            SubagentStarted,
            SubagentError,
            SubagentError,
            RunFinished
        ]
    );
}

/// covers: SUB-16, SUB-35
#[test]
fn a_suspended_invocation_suspends_its_still_active_descendants() {
    use agui_sdk::ids::InterruptId;
    use agui_sdk::subagent::SubagentFinishedOutcome;
    let runner = Runner::new(agent(|ctx| async move {
        let p = ctx.emitter().start_subagent("p").await?;
        let p_id = p.id().clone();
        let c = p.start_subagent("c").await?;
        std::mem::forget(c);
        p.finish(
            Some(SubagentFinishedOutcome::suspended([InterruptId::new("i1")])),
            None,
        )
        .await?;
        Ok(RunOutcome::interrupt([
            Interrupt::new("i1", "approval").with_subagent_run_id(p_id)
        ]))
    }));
    let events = run(&runner, input());
    assert_one_valid_run(&events);
    use EventType::*;
    let kinds = types(&events);
    let first = kinds.iter().position(|t| *t == SubagentStarted).unwrap();
    assert_eq!(
        kinds[first..],
        [
            SubagentStarted,
            SubagentStarted,
            SubagentFinished,
            SubagentFinished,
            RunFinished
        ]
    );
    let finished: Vec<_> = events
        .iter()
        .filter_map(|e| match e {
            Event::SubagentFinished(f) => Some(f),
            _ => None,
        })
        .collect();
    assert!(
        matches!(
            &finished[0].outcome,
            Some(SubagentFinishedOutcome::Suspended {
                interrupt_ids: None,
                ..
            })
        ),
        "child outcome: {:?}",
        finished[0].outcome
    );
    assert_eq!(
        finished[1].outcome,
        Some(SubagentFinishedOutcome::suspended([InterruptId::new("i1")]))
    );
    let mut verifier = ProducerVerifier::new();
    for event in &events {
        verifier.check(event).unwrap();
    }
}

/// covers: LIFE-12, STRM-26
#[test]
fn closing_more_items_than_the_queue_holds_is_backpressure_not_failure() {
    let mut config = RunnerConfig::default();
    config.queue_capacity = 1;
    let runner = Runner::new(agent(|ctx| async move {
        let sub = ctx.emitter().start_subagent("helper").await?;
        let step = sub.emitter().start_step("s").await?;
        let text = sub.emitter().start_text(TextMessageRole::Assistant).await?;
        std::mem::forget((step, text));
        sub.finish(None, None).await?;
        Ok(RunOutcome::success())
    }))
    .config(config);
    let events = run(&runner, input());
    assert_one_valid_run(&events);
    use EventType::*;
    assert_eq!(terminal(&events).event_type(), RunFinished);
    let kinds = types(&events);
    let pos = |t: EventType| kinds.iter().position(|k| *k == t).unwrap();
    assert!(pos(TextMessageEnd) < pos(StepFinished));
    assert!(pos(StepFinished) < pos(SubagentFinished));
}

#[test]
fn a_cancelled_outcome_drops_the_result() {
    let runner = Runner::new(agent(|_ctx| async {
        Ok(RunOutcome::cancelled().with_result(json!("ignored")))
    }));
    let events = run(&runner, input());
    let finished = finished(&events);
    assert_eq!(finished.outcome, Some(RunFinishedOutcome::cancelled()));
    assert!(finished.result.is_none());
}

#[test]
fn an_outside_cancel_wakes_a_suspended_run() {
    let runner = Runner::new(agent(|ctx| async move {
        let text = ctx.emitter().start_text(TextMessageRole::Assistant).await?;
        // Waiting on something that never wakes it: only the runner's own
        // registration with the token can end this run.
        std::future::pending::<()>().await;
        text.push("never").await?;
        Ok(RunOutcome::success())
    }));
    let cancel = CancelToken::new();
    let stream = start(&runner, input(), cancel.clone());
    let canceller = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(20));
        cancel.cancel();
    });
    let events = drain(stream);
    canceller.join().unwrap();
    assert_one_valid_run(&events);
    assert_eq!(
        finished(&events).outcome,
        Some(RunFinishedOutcome::cancelled())
    );
}

/// covers: LIFE-13, LIFE-21
#[test]
fn failures_end_with_one_run_error() {
    // A public SDK error reaches the client with its code and message.
    let runner = Runner::new(agent(|ctx| async move {
        let _open = ctx.emitter().start_text(TextMessageRole::Assistant).await?;
        Err(Error::new(ErrorCode::AgentFailed, "the model refused")
            .public()
            .into())
    }));
    let events = run(&runner, input());
    assert_one_valid_run(&events);
    let error = run_error(&events);
    assert_eq!(error.message, "the model refused");
    assert_eq!(error.code.as_deref(), Some("agent.failed"));

    // Any other error (here an application's own type via `?`) is generic.
    let runner = Runner::new(agent(|_ctx| async {
        let parsed: u32 = "not a number with a secret".parse()?;
        Ok(RunOutcome::success().with_result(json!(parsed)))
    }));
    let events = run(&runner, input());
    let error = run_error(&events);
    assert_eq!(error.code.as_deref(), Some("agent.failed"));
    assert!(!error.message.contains("secret"));
    let runner = Runner::new(agent(|_ctx| async {
        Err(Error::new(ErrorCode::Internal, "db password=hunter2").into())
    }));
    let events = run(&runner, input());
    assert!(!run_error(&events).message.contains("hunter2"));

    // A RUN_ERROR the agent wrote itself is sent as given, with the run's usage.
    let runner = Runner::new(agent(|ctx| async move {
        ctx.emitter().record_usage(usage_of("anthropic", 1, 1));
        let mut error = RunError::new("quota exhausted, try tomorrow");
        error.code = Some("x-acme:quota".into());
        Ok(RunOutcome::failed(error))
    }));
    let events = run(&runner, input());
    let error = run_error(&events);
    assert_eq!(error.message, "quota exhausted, try tomorrow");
    assert_eq!(error.code.as_deref(), Some("x-acme:quota"));
    assert_eq!(error.usage.as_ref().map(Vec::len), Some(1));

    let runner = Runner::new(agent(|_ctx| async {
        if true {
            panic!("secret detail");
        }
        Ok(RunOutcome::success())
    }));
    let events = run(&runner, input());
    let error = run_error(&events);
    assert_eq!(error.code.as_deref(), Some("agent.panicked"));
    assert!(!error.message.contains("secret"), "panic payload leaked");
}

/// covers: LIFE-21, ARCH-04
#[test]
fn an_agents_run_error_is_verified() {
    let runner = Runner::new(agent(|_ctx| async {
        let mut error = RunError::new("quota exhausted");
        error.usage = Some(vec![TokenUsage::default()
            .with_provider("x")
            .with_model("m")
            .with_input_tokens(1)
            .with_output_tokens(1)
            .with_total_tokens(5)]);
        Ok(RunOutcome::failed(error))
    }));
    let events = run(&runner, input());
    let error = run_error(&events);
    assert_eq!(error.code.as_deref(), Some("server.producer_violation"));
    assert_one_valid_run(&events);
    let mut verifier = ProducerVerifier::new();
    for event in &events {
        verifier.check(event).unwrap();
    }
}

/// covers: LIFE-21, ARCH-04
#[test]
fn recorded_usage_the_verifier_refuses_never_reaches_a_failure_run_error() {
    let runner = Runner::new(agent(|ctx| async move {
        ctx.emitter().record_usage(
            TokenUsage::default()
                .with_provider("x")
                .with_model("m")
                .with_input_tokens(1)
                .with_cached_input_tokens(9),
        );
        Err(Error::new(ErrorCode::AgentFailed, "boom").into())
    }));
    let events = run(&runner, input());
    assert_one_valid_run(&events);
    let mut verifier = ProducerVerifier::new();
    for event in &events {
        verifier.check(event).unwrap();
    }
}

/// covers: LIFE-21, ARCH-04
#[test]
fn recorded_usage_the_verifier_refuses_never_reaches_an_interrupt_refusal_run_error() {
    let runner = Runner::new(agent(|ctx| async move {
        ctx.emitter().record_usage(
            TokenUsage::default()
                .with_provider("x")
                .with_model("m")
                .with_input_tokens(1)
                .with_cached_input_tokens(9),
        );
        Ok(RunOutcome::interrupt(Vec::new()))
    }));
    let events = run(&runner, input());
    assert_one_valid_run(&events);
    assert_eq!(terminal(&events).event_type(), EventType::RunError);
    let mut verifier = ProducerVerifier::new();
    for event in &events {
        verifier.check(event).unwrap();
    }
}

/// covers: SUB-16, INTR-06, ARCH-04
#[test]
fn an_invalid_interrupt_list_suspends_no_open_invocation() {
    let runner = Runner::new(agent(|ctx| async move {
        let p = ctx.emitter().start_subagent("p").await?;
        let p_id = p.id().clone();
        std::mem::forget(p);
        Ok(RunOutcome::interrupt([
            Interrupt::new("i1", "approval").with_subagent_run_id(p_id.clone()),
            Interrupt::new("i1", "approval").with_subagent_run_id(p_id),
        ]))
    }));
    let events = run(&runner, input());
    assert_one_valid_run(&events);
    assert_eq!(terminal(&events).event_type(), EventType::RunError);
    assert!(
        !events
            .iter()
            .any(|e| e.event_type() == EventType::SubagentFinished),
        "no invocation may be reported suspended on interrupts never delivered"
    );
    let mut verifier = ProducerVerifier::new();
    for event in &events {
        verifier.check(event).unwrap();
    }
}

/// covers: SUB-16, INTR-06, LIFE-21, ARCH-04
#[test]
fn a_refused_interrupt_run_finished_suspends_no_open_invocation() {
    let runner = Runner::new(agent(|ctx| async move {
        ctx.emitter().record_usage(
            TokenUsage::default()
                .with_provider("x")
                .with_model("m")
                .with_input_tokens(1)
                .with_cached_input_tokens(9),
        );
        let p = ctx.emitter().start_subagent("p").await?;
        let p_id = p.id().clone();
        std::mem::forget(p);
        Ok(RunOutcome::interrupt([
            Interrupt::new("i1", "approval").with_subagent_run_id(p_id)
        ]))
    }));
    let events = run(&runner, input());
    assert_one_valid_run(&events);
    assert_eq!(terminal(&events).event_type(), EventType::RunError);
    assert!(
        !events
            .iter()
            .any(|e| e.event_type() == EventType::SubagentFinished),
        "a failed run reports no invocation suspended on interrupts never delivered"
    );
    let mut verifier = ProducerVerifier::new();
    for event in &events {
        verifier.check(event).unwrap();
    }
}

/// covers: LIFE-37, LIFE-41
#[test]
fn nothing_is_sent_after_the_terminal() {
    let kept: Arc<Mutex<Option<Emitter>>> = Arc::new(Mutex::new(None));
    let kept2 = Arc::clone(&kept);
    let runner = Runner::new(agent(move |ctx| {
        let kept = Arc::clone(&kept2);
        async move {
            *kept.lock().unwrap() = Some(ctx.emitter().clone());
            Ok(RunOutcome::success())
        }
    }));
    let events = run(&runner, input());
    assert_eq!(terminal(&events).event_type(), EventType::RunFinished);
    let emitter = kept.lock().unwrap().take().unwrap();
    let late = futures_executor::block_on(emitter.say("late"));
    assert_eq!(late.err().map(|e| e.code()), Some(ErrorCode::Disconnected));
}

/// covers: LIFE-38, LIFE-43, LIFE-44, LIFE-46
#[test]
fn replayed_runs_are_restated_as_snapshots_before_the_requested_run() {
    let runner = Runner::new(agent(|ctx| async move {
        ctx.emitter().say("third").await?;
        Ok(RunOutcome::success())
    }));
    let replay = vec![
        ReplayedRun::new(
            RunId::new("r-a"),
            vec![Message::user("one")],
            json!({"n": 1}),
        ),
        ReplayedRun::new(
            RunId::new("r-b"),
            vec![Message::user("two")],
            json!({"n": 2}),
        ),
    ];
    let events = drain(
        runner
            .run_with_replay(
                replay,
                admitted(input()),
                CancelToken::new(),
                Extensions::new(),
            )
            .unwrap(),
    );
    let kinds = types(&events);
    use EventType::*;
    assert_eq!(
        kinds[..8],
        [
            RunStarted,
            MessagesSnapshot,
            StateSnapshot,
            RunFinished,
            RunStarted,
            MessagesSnapshot,
            StateSnapshot,
            RunFinished
        ]
    );
    assert_eq!(kinds[8], RunStarted);
    assert_eq!(*kinds.last().unwrap(), RunFinished);
    // No content is re-streamed for the earlier runs.
    assert!(!kinds[..8].contains(&TextMessageContent));
    let run_ids: Vec<_> = events
        .iter()
        .filter_map(|e| match e {
            Event::RunStarted(s) => Some(s.run_id.as_str().to_owned()),
            _ => None,
        })
        .collect();
    assert_eq!(run_ids, ["r-a", "r-b", "r1"]);
}

/// covers: LIFE-43, LIFE-13
#[test]
fn a_refused_replay_says_why() {
    let runner = Runner::new(agent(|_ctx| async { Ok(RunOutcome::success()) }));
    let replay = |ids: [&str; 2]| {
        ids.map(|id| ReplayedRun::new(RunId::new(id), vec![Message::user("x")], json!({})))
            .to_vec()
    };
    let events = drain(
        runner
            .run_with_replay(
                replay(["r-a", "r-a"]),
                admitted(input()),
                CancelToken::new(),
                Extensions::new(),
            )
            .unwrap(),
    );
    // What the verifier itself says about the same sequence.
    let mut verifier = ProducerVerifier::new();
    let mut reason = None;
    for run in replay(["r-a", "r-a"]) {
        for event in [
            Event::from(RunStarted::new("t1", run.run_id.clone())),
            Event::from(RunFinished::new("t1", run.run_id.clone())),
        ] {
            if let Err(v) = verifier.check(&event) {
                reason.get_or_insert(v.message);
            }
        }
    }
    let reason = reason.expect("the verifier refuses a repeated run id");
    let error = run_error(&events);
    assert!(
        error.message.contains(&reason),
        "{:?} should contain {reason:?}",
        error.message
    );
}

/// A run that fails before it begins, after a replay whose run id it
/// repeats: the RUN_ERROR says why the run could not even be opened.
#[test]
fn a_startup_failure_after_a_refused_run_started_names_the_refusal() {
    struct Passthrough;
    impl Transformer for Passthrough {
        fn transform(&mut self, event: Event, out: &mut Vec<Event>) {
            out.push(event);
        }
    }
    let runner = Runner::new(agent(|_ctx| async { Ok(RunOutcome::success()) }))
        .transformer(|| -> Passthrough { panic!("factory fault") });
    let replay = vec![ReplayedRun::new(RunId::new("r1"), Vec::new(), json!({}))];
    let events = drain(
        runner
            .run_with_replay(
                replay,
                admitted(input()),
                CancelToken::new(),
                Extensions::new(),
            )
            .unwrap(),
    );
    let error = run_error(&events);
    assert_eq!(
        error.code.as_deref(),
        Some(ErrorCode::ProducerViolation.as_str()),
        "{error:?}"
    );
    assert!(
        error.message.starts_with("RUN_STARTED was refused"),
        "{error:?}"
    );
}

/// covers: LIFE-39
#[test]
fn only_a_late_run_error_may_follow_a_finished_run() {
    let mut verifier = ProducerVerifier::new();
    verifier.check(&RunStarted::new("t1", "r1").into()).unwrap();
    verifier
        .check(&RunFinished::new("t1", "r1").into())
        .unwrap();
    let refused = verifier
        .check(&Event::from(StepStarted::new("late")))
        .unwrap_err();
    assert_eq!(refused.rule, ProducerRule::AfterClose);
    verifier
        .check(&RunError::new("flush failed").into())
        .unwrap();
}

/// An empty `pendingToolCallIds` means "named none" and
/// the consumer derives the list (schema.mdx), so it is not compared.
/// covers: TOOL-29, TOOL-31
#[test]
fn an_empty_pending_list_is_the_same_as_none_and_a_wrong_one_is_refused() {
    fn finish_with(pending: Vec<&str>) -> Result<(), ProducerRule> {
        let mut verifier = ProducerVerifier::new();
        verifier.check(&RunStarted::new("t1", "r1").into()).unwrap();
        verifier
            .check(&agui_sdk::tool::ToolCallStart::new("c1", "confirm").into())
            .unwrap();
        verifier
            .check(&agui_sdk::tool::ToolCallEnd::new("c1").into())
            .unwrap();
        let ids: Vec<ToolCallId> = pending.into_iter().map(ToolCallId::new).collect();
        let outcome = RunFinishedOutcome::success_with_pending(ids);
        verifier
            .check(&RunFinished::new("t1", "r1").with_outcome(outcome).into())
            .map_err(|v| v.rule)
    }
    assert_eq!(finish_with(vec![]), Ok(()));
    assert_eq!(finish_with(vec!["c1"]), Ok(()));
    assert_eq!(finish_with(vec!["other"]), Err(ProducerRule::Outcome));
}

/// covers: LIFE-49, LIFE-50
#[test]
fn steps_open_once_and_finish_only_when_open() {
    let runner = Runner::new(agent(|ctx| async move {
        let _plan = ctx.emitter().start_step("plan").await?;
        let again = ctx.emitter().start_step("plan").await;
        assert_eq!(
            again.err().map(|e| e.code()),
            Some(ErrorCode::ProducerViolation)
        );
        Ok(RunOutcome::success())
    }));
    let events = run(&runner, input());
    assert_eq!(
        run_error(&events).code.as_deref(),
        Some("server.producer_violation")
    );

    let runner = Runner::new(agent(|ctx| async move {
        ctx.emitter()
            .emit(StepFinished::new("never-opened"))
            .await?;
        Ok(RunOutcome::success())
    }));
    let events = run(&runner, input());
    assert_eq!(types(&events), [EventType::RunStarted, EventType::RunError]);
}

/// covers: LIFE-51, LIFE-52
#[test]
fn steps_overlap_and_are_closed_before_the_finish() {
    let runner = Runner::new(agent(|ctx| async move {
        let e = ctx.emitter();
        let a = e.start_step("a").await?;
        let _b = e.start_step("b").await?;
        e.say("inside both").await?;
        a.finish().await?; // a closes while b is still open: overlap, not nesting
        Ok(RunOutcome::success()) // b left open: the runner closes it
    }));
    let events = run(&runner, input());
    assert_one_valid_run(&events);
    let kinds = types(&events);
    let n = kinds.len();
    assert_eq!(kinds[n - 2], EventType::StepFinished);
    let Event::StepFinished(last) = &events[n - 2] else {
        panic!()
    };
    assert_eq!(last.step_name, "b");
}

/// covers: CHG-01
#[test]
fn chunks_pass_through_with_their_first_chunk_fields() {
    let runner = Runner::new(agent(|ctx| async move {
        let mut first = TextMessageChunk::new();
        first.message_id = Some("m1".into());
        first.role = Some(TextMessageRole::Assistant);
        first.delta = Some("Hel".into());
        let mut later = TextMessageChunk::new();
        later.message_id = Some("m1".into());
        later.delta = Some("lo".into());
        ctx.emitter().emit(first).await?;
        ctx.emitter().emit(later).await?;
        Ok(RunOutcome::success())
    }));
    let events = run(&runner, input());
    let chunks: Vec<_> = events
        .iter()
        .filter_map(|e| match e {
            Event::TextMessageChunk(c) => Some(c.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(chunks.len(), 2);
    assert!(chunks[0].role.is_some() && chunks[1].role.is_none());
}

// --- Exactly one terminal, whatever fails --------------------------------

mod failure_injection {
    use super::*;
    use proptest::prelude::*;

    /// One way a run can go wrong, or not.
    #[derive(Debug, Clone)]
    enum Fault {
        None,
        AgentError,
        AgentPanic,
        Violation,
        Overflow,
        CancelInside,
        CancelOutside,
        TransformerPanic,
        StorePanic,
        BadInterrupts,
        EarlyDrop,
    }

    fn fault() -> impl Strategy<Value = Fault> {
        prop_oneof![
            Just(Fault::None),
            Just(Fault::AgentError),
            Just(Fault::AgentPanic),
            Just(Fault::Violation),
            Just(Fault::Overflow),
            Just(Fault::CancelInside),
            Just(Fault::CancelOutside),
            Just(Fault::TransformerPanic),
            Just(Fault::StorePanic),
            Just(Fault::BadInterrupts),
            Just(Fault::EarlyDrop),
        ]
    }

    /// An interrupt store whose backend blows up.
    struct PanickyStore;

    impl InterruptStore for PanickyStore {
        async fn record(
            &self,
            _: &agui_sdk::ids::ThreadId,
            _: Vec<Interrupt>,
        ) -> agui_sdk::Result<()> {
            panic!("store fault")
        }
        async fn check(
            &self,
            _: &agui_sdk::ids::ThreadId,
            _: &[agui_sdk::interrupt::ResumeEntry],
        ) -> agui_sdk::Result<agui_sdk::server::helpers::ResumeCheck> {
            panic!("store fault")
        }
        async fn resolve(
            &self,
            _: &agui_sdk::ids::ThreadId,
            _: &agui_sdk::ids::InterruptId,
        ) -> agui_sdk::Result<()> {
            panic!("store fault")
        }
    }

    /// Panics on the `n`th event it sees when armed.
    struct Faulty {
        armed: bool,
        at: usize,
        seen: usize,
        expand: usize,
    }

    impl Transformer for Faulty {
        fn transform(&mut self, event: Event, out: &mut Vec<Event>) {
            self.seen += 1;
            if self.armed && self.seen == self.at {
                panic!("transformer fault");
            }
            if self.expand > 1 {
                if let Event::Custom(_) = &event {
                    for _ in 1..self.expand {
                        out.push(event.clone());
                    }
                }
            }
            out.push(event);
        }
    }

    async fn script(
        ctx: agui_sdk::server::RunContext,
        fault: Fault,
        steps: usize,
        at: usize,
    ) -> Result<RunOutcome, agui_sdk::server::BoxError> {
        let e = ctx.emitter().clone();
        let mut open = Vec::new();
        for i in 0..steps {
            if i == at {
                match fault {
                    Fault::AgentError => {
                        return Err(Error::new(ErrorCode::AgentFailed, "fault").into())
                    }
                    Fault::StorePanic => {
                        let _ = PanickyStore.check(ctx.thread_id(), ctx.resume()).await;
                    }
                    Fault::AgentPanic => panic!("agent fault"),
                    Fault::Violation => {
                        let _ = e.emit(StepFinished::new("never-opened")).await;
                    }
                    Fault::Overflow => {
                        let _ = e.custom("x:expand", json!(i)).await;
                    }
                    Fault::CancelInside => ctx.cancellation().cancel(),
                    _ => {}
                }
            }
            match i % 5 {
                0 => {
                    let t = e.start_text(TextMessageRole::Assistant).await?;
                    t.push("a").await?;
                    open.push(Some(t));
                }
                1 => {
                    e.say("b").await?;
                }
                2 => {
                    let c = e.start_tool_call("tool", None).await?;
                    c.push_args("{").await?;
                    std::mem::forget(c);
                }
                3 => {
                    if let Some(Some(t)) = open.pop() {
                        t.end().await?;
                    }
                }
                _ => {
                    let s = e.start_subagent("sub").await?;
                    s.emitter().say("c").await?;
                    if i % 2 == 0 {
                        s.finish(None, None).await?;
                    } else {
                        std::mem::forget(s);
                    }
                }
            }
        }
        std::mem::forget(open);
        if let Fault::BadInterrupts = fault {
            return Ok(RunOutcome::interrupt([
                Interrupt::new("dup", "a"),
                Interrupt::new("dup", "b"),
            ]));
        }
        Ok(RunOutcome::success())
    }

    /// A send the transformers expand past the queue's capacity ends the run
    /// with `server.queue_overflow` (the property test below only checks
    /// that some valid terminal arrives).
    #[test]
    fn an_expansion_past_the_queue_capacity_is_a_queue_overflow() {
        let mut config = RunnerConfig::default();
        config.queue_capacity = 2;
        let runner = Runner::new(agent(|ctx| async move {
            let _ = ctx.emitter().custom("x:expand", json!(1)).await;
            Ok(RunOutcome::success())
        }))
        .config(config)
        .diagnostic_sink(agui_sdk::diagnostics::NullSink)
        .transformer(|| Faulty {
            armed: false,
            at: 0,
            seen: 0,
            expand: 3,
        });
        let events = run(&runner, input());
        assert_one_valid_run(&events);
        assert_eq!(
            run_error(&events).code.as_deref(),
            Some("server.queue_overflow")
        );
    }

    /// A panicking transformer ends the run with `agent.panicked`.
    #[test]
    fn a_transformer_panic_is_an_agent_panic() {
        let runner = Runner::new(agent(|ctx| async move {
            let _ = ctx.emitter().say("a").await;
            Ok(RunOutcome::success())
        }))
        .diagnostic_sink(agui_sdk::diagnostics::NullSink)
        .transformer(|| Faulty {
            armed: true,
            at: 1,
            seen: 0,
            expand: 1,
        });
        let events = run(&runner, input());
        assert_one_valid_run(&events);
        assert_eq!(run_error(&events).code.as_deref(), Some("agent.panicked"));
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(256))]

        #[test]
        fn every_run_ends_with_exactly_one_valid_terminal(
            fault in fault(),
            steps in 0usize..24,
            at in 0usize..24,
            capacity in 1usize..8,
        ) {
            let config = {
                let mut c = RunnerConfig::default();
                c.queue_capacity = capacity;
                c
            };
            let armed = matches!(fault, Fault::TransformerPanic);
            let expand = if matches!(fault, Fault::Overflow) { capacity + 1 } else { 1 };
            let fault_for_agent = fault.clone();
            let runner = Runner::new(agent(move |ctx| {
                let fault = fault_for_agent.clone();
                async move { script(ctx, fault, steps, at).await }
            }))
            .config(config)
            .diagnostic_sink(agui_sdk::diagnostics::NullSink)
            .transformer(move || Faulty { armed, at: at + 1, seen: 0, expand });
            let cancel = CancelToken::new();
            let mut stream = start(&runner, input(), cancel.clone());
            let events = futures_executor::block_on(async {
                let mut out = Vec::new();
                let mut polls = 0usize;
                while let Some(event) = stream.next().await {
                    polls += 1;
                    if polls == at + 1 {
                        if let Fault::CancelOutside = fault { cancel.cancel(); }
                    }
                    out.push(event);
                    if polls == at + 1 {
                        if let Fault::EarlyDrop = fault { break; }
                    }
                }
                out
            });
            if let Fault::EarlyDrop = fault {
                // A consumer that walks away gets a truncated stream, never
                // a synthesized terminal; nothing more is required.
                prop_assert!(events.iter().filter(|e| e.is_run_terminal()).count() <= 1);
                return Ok(());
            }
            assert_one_valid_run(&events);
            let terminal = terminal(&events).clone();
            match fault {
                Fault::AgentError | Fault::AgentPanic | Fault::StorePanic | Fault::BadInterrupts
                    if at < steps || matches!(fault, Fault::BadInterrupts) =>
                {
                    prop_assert_eq!(terminal.event_type(), EventType::RunError);
                }
                Fault::CancelInside if at < steps => {
                    let is_cancelled = matches!(
                        &terminal,
                        Event::RunFinished(f) if f.outcome == Some(RunFinishedOutcome::cancelled())
                    );
                    prop_assert!(is_cancelled);
                }
                Fault::None => prop_assert_eq!(terminal.event_type(), EventType::RunFinished),
                _ => {}
            }
        }
    }

    #[test]
    fn pending_ids_match_unanswered_calls_after_closers() {
        let runner = Runner::new(agent(|ctx| async move {
            let a = ctx.emitter().start_tool_call("x", None).await?;
            let b = ctx.emitter().start_tool_call("y", None).await?;
            let b_id = b.id().clone();
            ctx.emitter().tool_result(b_id, "done").await?;
            std::mem::forget((a, b));
            Ok(RunOutcome::success())
        }));
        let events = run(&runner, input());
        let Event::ToolCallStart(a) = &events[1] else {
            panic!()
        };
        let pending: Vec<ToolCallId> = vec![a.tool_call_id.clone()];
        assert_eq!(
            finished(&events).outcome,
            Some(RunFinishedOutcome::success_with_pending(pending))
        );
    }
}

/// An agent error that wraps the cancellation its send reported.
#[derive(Debug)]
struct Wrapped(Error);

impl std::fmt::Display for Wrapped {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("the agent wound down")
    }
}

impl std::error::Error for Wrapped {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.0)
    }
}

/// covers: LIFE-11, LIFE-16
#[test]
fn a_wrapped_cancellation_still_finishes_cancelled() {
    let runner = Runner::new(agent(|ctx| async move {
        ctx.cancellation().cancel();
        match ctx.emitter().say("too late").await {
            Err(e) => Err(Wrapped(e).into()),
            Ok(_) => Ok(RunOutcome::success()),
        }
    }));
    let events = run(&runner, input());
    assert_one_valid_run(&events);
    assert_eq!(
        finished(&events).outcome,
        Some(RunFinishedOutcome::cancelled())
    );
}
