//! Token usage (`events/lifecycle.mdx`, "Token usage"): the wire type and
//! the producer helper that keeps the accounting rules.

// Test code: a failed unwrap is a failed test.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use agui_sdk::event::Event;
use agui_sdk::usage::{TokenUsage, TokenUsageBuilder};
use serde_json::json;

#[cfg(feature = "server")]
mod support_server;

/// covers: LIFE-25
#[test]
fn input_counts_every_charged_prompt_token_and_output_includes_reasoning() {
    // Provider reports uncached input beside cache reads and writes, and
    // output beside reasoning: the totals take every part in.
    let usage = TokenUsageBuilder::new("anthropic", "claude-x")
        .input(40, false)
        .cached_input(60)
        .cache_write_input(20)
        .output(30, false)
        .reasoning(10)
        .build();
    assert_eq!(usage.input_tokens, Some(120));
    assert_eq!(usage.output_tokens, Some(40));
}

/// covers: LIFE-26
#[test]
fn parts_never_add_to_the_total() {
    let usage = TokenUsageBuilder::new("openai", "gpt-x")
        .input(100, true)
        .cached_input(60)
        .cache_write_input(20)
        .output(50, true)
        .reasoning(10)
        .build();
    assert_eq!(usage.input_tokens, Some(100));
    assert_eq!(usage.cached_input_tokens, Some(60));
    assert_eq!(usage.cache_write_input_tokens, Some(20));
    assert_eq!(usage.output_tokens, Some(50));
    assert_eq!(usage.reasoning_tokens, Some(10));
    assert_eq!(usage.total_tokens, Some(150));
}

/// covers: LIFE-27
#[test]
fn a_part_reported_beside_a_smaller_total_is_added_in() {
    let usage = TokenUsageBuilder::new("bedrock", "m")
        .input(400, false)
        .cached_input(600)
        .build();
    assert_eq!(usage.input_tokens, Some(1000));
    assert_eq!(usage.cached_input_tokens, Some(600));
}

/// covers: LIFE-28
#[test]
fn a_part_already_inside_the_total_is_not_added_again() {
    let usage = TokenUsageBuilder::new("google", "m")
        .input(1000, true)
        .cached_input(600)
        .build();
    assert_eq!(usage.input_tokens, Some(1000));
    let usage = TokenUsageBuilder::new("google", "m")
        .output(80, true)
        .reasoning(30)
        .build();
    assert_eq!(usage.output_tokens, Some(80));
}

/// covers: LIFE-29
#[test]
fn total_is_input_plus_output() {
    let usage = TokenUsageBuilder::new("mistral", "m")
        .input(7, true)
        .output(5, true)
        .build();
    assert_eq!(usage.total_tokens, Some(12));
}

/// covers: LIFE-30
#[test]
fn total_is_computed_rather_than_copied() {
    let usage = TokenUsageBuilder::new("azure", "m")
        .input(10, true)
        .output(5, true)
        .absorb(
            &TokenUsage::default()
                .with_input_tokens(3)
                .with_output_tokens(2)
                .with_total_tokens(999),
        )
        .build();
    assert_eq!(usage.total_tokens, Some(20));
}

/// covers: LIFE-31
#[test]
fn a_differently_counted_provider_total_is_never_copied() {
    // A provider total that excludes cache reads (here 40) never reaches the
    // wire; the builder has no way to set it and derives its own.
    let provider_total_excluding_cache = 40;
    let usage = TokenUsageBuilder::new("openai", "m")
        .input(30, false)
        .cached_input(20)
        .output(10, true)
        .build();
    assert_eq!(usage.total_tokens, Some(60));
    assert_ne!(usage.total_tokens, Some(provider_total_excluding_cache));
}

/// covers: LIFE-32
#[test]
fn no_zero_for_a_count_without_data() {
    let usage = TokenUsageBuilder::new("openai", "m")
        .output(5, true)
        .build();
    assert_eq!(usage.input_tokens, None);
    assert_eq!(usage.total_tokens, None);
    assert_eq!(usage.cached_input_tokens, None);
    // A part alone does not invent a total.
    let usage = TokenUsageBuilder::new("openai", "m")
        .cached_input(5)
        .build();
    assert_eq!(usage.input_tokens, None);
    assert_eq!(usage.cached_input_tokens, Some(5));
    let value = serde_json::to_value(&usage).unwrap();
    assert_eq!(
        value,
        json!({"provider": "openai", "model": "m", "cachedInputTokens": 5})
    );
    // A reported zero is data.
    let zero = TokenUsageBuilder::new("openai", "m")
        .input(0, true)
        .output(0, true)
        .build();
    assert_eq!(zero.total_tokens, Some(0));
}

/// covers: LIFE-33
#[test]
fn absent_counts_stay_absent_when_read() {
    let event: Event = serde_json::from_value(json!({
        "type": "RUN_FINISHED", "threadId": "t", "runId": "r",
        "usage": [{"provider": "openai", "model": "m", "outputTokens": 0}]
    }))
    .unwrap();
    let Event::RunFinished(finished) = event else {
        panic!("variant")
    };
    let usage = &finished.usage.unwrap()[0];
    assert_eq!(usage.input_tokens, None);
    assert_eq!(usage.total_tokens, None);
    assert_eq!(usage.output_tokens, Some(0));
}

/// covers: LIFE-34
#[test]
fn subagent_calls_are_part_of_the_run() {
    let parent_call = TokenUsageBuilder::new("anthropic", "m")
        .input(100, true)
        .output(10, true)
        .build();
    let subagent_call = TokenUsageBuilder::new("anthropic", "m")
        .input(50, true)
        .output(5, true)
        .build();
    let run = TokenUsageBuilder::new("anthropic", "m")
        .absorb(&parent_call)
        .absorb(&subagent_call)
        .build();
    assert_eq!(run.input_tokens, Some(150));
    assert_eq!(run.total_tokens, Some(165));
}

// LIFE-35 and LIFE-36 are about what the runner puts on each run's terminal
// event, so they drive `server::Runner` rather than the builder.

/// covers: LIFE-35
#[cfg(feature = "server")]
#[test]
fn a_separate_run_reports_its_own_usage() {
    use std::sync::{Arc, Mutex};

    use agui_sdk::server::{CancelToken, Extensions, RunOutcome, Runner};
    use agui_sdk::{Message, RunAgentInput};
    use support_server::{admitted, agent, finished, input, run};

    let child = Arc::new(Runner::new(agent(|ctx| async move {
        ctx.emitter().record_usage(
            TokenUsageBuilder::new("openai", "m")
                .input(70, true)
                .output(7, true)
                .build(),
        );
        Ok(RunOutcome::success())
    })));
    let child_events = Arc::new(Mutex::new(Vec::new()));
    let (spawn, log) = (Arc::clone(&child), Arc::clone(&child_events));
    let parent = Runner::new(agent(move |ctx| {
        let (child, log) = (Arc::clone(&spawn), Arc::clone(&log));
        async move {
            ctx.emitter().record_usage(
                TokenUsageBuilder::new("openai", "m")
                    .input(10, true)
                    .output(1, true)
                    .build(),
            );
            // Another agent invoked as a separate run, named by parentRunId.
            let sub_input = RunAgentInput::new(
                ctx.thread_id().clone(),
                "r2",
                vec![Message::user("sub-task")],
            )
            .with_protocol_version("1.0")
            .with_parent_run_id(ctx.run_id().clone());
            let mut stream =
                child.run(admitted(sub_input), CancelToken::new(), Extensions::new())?;
            while let Some(event) = stream.next().await {
                log.lock().unwrap().push(event);
            }
            ctx.emitter().record_usage(
                TokenUsageBuilder::new("openai", "m")
                    .input(2, true)
                    .output(0, true)
                    .build(),
            );
            Ok(RunOutcome::success())
        }
    }));
    let events = run(&parent, input());
    let child_events = child_events.lock().unwrap().clone();

    // The child names its parent and reports its own calls...
    let Some(Event::RunStarted(started)) = child_events.first() else {
        panic!("child stream: {child_events:?}")
    };
    assert_eq!(
        started.parent_run_id.as_ref().map(|r| r.as_str()),
        Some("r1")
    );
    let own = finished(&child_events).usage.clone().unwrap();
    assert_eq!(own.len(), 1);
    assert_eq!(
        (
            own[0].input_tokens,
            own[0].output_tokens,
            own[0].total_tokens
        ),
        (Some(70), Some(7), Some(77))
    );
    // ...and the parent's terminal has only the parent's: 10+1 and 2+0.
    let parent_usage = finished(&events).usage.clone().unwrap();
    assert_eq!(parent_usage.len(), 1);
    assert_eq!(
        (
            parent_usage[0].input_tokens,
            parent_usage[0].output_tokens,
            parent_usage[0].total_tokens
        ),
        (Some(12), Some(1), Some(13))
    );
}

/// covers: LIFE-36
#[cfg(feature = "server")]
#[test]
fn a_resuming_run_reports_only_its_own_calls() {
    use agui_sdk::interrupt::{Interrupt, ResumeEntry};
    use agui_sdk::server::{RunOutcome, Runner};
    use support_server::{agent, finished, input, run, run_error};

    // The interrupted run reports its calls; the resuming run, which ends
    // in RUN_ERROR, reports only the calls it made itself.
    let runner = Runner::new(agent(|ctx| async move {
        if ctx.resume().is_empty() {
            ctx.emitter().record_usage(
                TokenUsageBuilder::new("openai", "m")
                    .input(100, true)
                    .output(20, true)
                    .build(),
            );
            Ok(RunOutcome::interrupt([Interrupt::new("i", "ask")]))
        } else {
            ctx.emitter().record_usage(
                TokenUsageBuilder::new("openai", "m")
                    .input(30, true)
                    .output(5, true)
                    .build(),
            );
            Err("late failure".into())
        }
    }));
    let interrupted = run(&runner, input());
    let first = finished(&interrupted).usage.clone().unwrap();
    assert_eq!(first[0].total_tokens, Some(120));
    let resumed = run(
        &runner,
        agui_sdk::RunAgentInput::new("t1", "r2", vec![agui_sdk::Message::user("yes")])
            .with_protocol_version("1.0")
            .with_resume([ResumeEntry::resolved("i", json!(true))]),
    );
    let second = run_error(&resumed).usage.clone().unwrap();
    assert_eq!(second.len(), 1);
    assert_eq!(
        (
            second[0].input_tokens,
            second[0].output_tokens,
            second[0].total_tokens
        ),
        (Some(30), Some(5), Some(35))
    );
    // A thread total is the consumer's sum across the thread's runs.
    let thread_total: u64 = [&first, &second]
        .iter()
        .flat_map(|u| u.iter().filter_map(|e| e.total_tokens))
        .sum();
    assert_eq!(thread_total, 155);
}
