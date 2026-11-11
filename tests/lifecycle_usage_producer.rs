//! Token usage accounting. The server half: building a rule-abiding
//! `TokenUsage` from provider counts, and what the runner reports.

#![cfg(feature = "server")]
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support_server;

use agui_sdk::event::Event;
use agui_sdk::server::{RunOutcome, Runner};
use agui_sdk::usage::{TokenUsage, TokenUsageBuilder};

use support_server::*;

/// covers: LIFE-25, LIFE-26
#[test]
fn totals_include_their_parts() {
    let usage = TokenUsageBuilder::new("anthropic", "m")
        .input(1200, true)
        .cached_input(1000)
        .cache_write_input(100)
        .output(300, true)
        .reasoning(200)
        .build();
    assert_eq!(usage.input_tokens, Some(1200));
    assert_eq!(usage.output_tokens, Some(300));
    assert_eq!(usage.cached_input_tokens, Some(1000));
    assert_eq!(usage.reasoning_tokens, Some(200));
    assert_eq!(usage.total_tokens, Some(1500));
}

/// covers: LIFE-27
#[test]
fn a_part_beside_a_smaller_total_is_added_in_once() {
    let usage = TokenUsageBuilder::new("openai", "m")
        .input(200, false)
        .cached_input(1000)
        .output(10, false)
        .reasoning(40)
        .build();
    assert_eq!(usage.input_tokens, Some(1200));
    assert_eq!(usage.output_tokens, Some(50));
    assert_eq!(usage.total_tokens, Some(1250));
}

/// covers: LIFE-28
#[test]
fn a_part_already_in_the_total_is_not_added_again() {
    let usage = TokenUsageBuilder::new("x", "m")
        .cached_input(1000)
        .input(1200, true)
        .build();
    assert_eq!(usage.input_tokens, Some(1200));
}

/// covers: LIFE-29, LIFE-30, LIFE-31
#[test]
fn the_total_is_computed_never_copied() {
    let usage = TokenUsageBuilder::new("x", "m")
        .input(7, true)
        .output(5, true)
        .build();
    assert_eq!(usage.total_tokens, Some(12));

    // A provider total that counts differently is not carried into the run.
    let runner = Runner::new(agent(|ctx| async move {
        ctx.emitter().record_usage(
            TokenUsage::default()
                .with_provider("x")
                .with_model("m")
                .with_input_tokens(7)
                .with_output_tokens(5)
                .with_total_tokens(999),
        );
        Ok(RunOutcome::success())
    }));
    let events = run(&runner, input());
    assert_eq!(
        finished(&events).usage.as_ref().unwrap()[0].total_tokens,
        Some(12)
    );
}

/// covers: LIFE-32
#[test]
fn unknown_counts_stay_absent() {
    let usage = TokenUsageBuilder::new("x", "m").input(10, true).build();
    assert_eq!(usage.output_tokens, None);
    assert_eq!(usage.total_tokens, None);
    assert_eq!(usage.cached_input_tokens, None);
    let runner = Runner::new(agent(|ctx| async move {
        ctx.emitter().record_usage(
            TokenUsage::default()
                .with_provider("x")
                .with_model("m")
                .with_input_tokens(1),
        );
        ctx.emitter().record_usage(
            TokenUsage::default()
                .with_provider("x")
                .with_model("m")
                .with_input_tokens(2),
        );
        Ok(RunOutcome::success())
    }));
    let events = run(&runner, input());
    let usage = &finished(&events).usage.as_ref().unwrap()[0];
    assert_eq!(usage.input_tokens, Some(3));
    assert_eq!(usage.output_tokens, None);
    assert_eq!(usage.total_tokens, None);
}

/// covers: LIFE-32
#[test]
fn a_merged_count_is_known_only_if_every_record_reported_it() {
    let merged = |records: Vec<TokenUsage>| -> TokenUsage {
        let runner = Runner::new(agent(move |ctx| {
            let records = records.clone();
            async move {
                for r in records {
                    ctx.emitter().record_usage(r);
                }
                Ok(RunOutcome::success())
            }
        }));
        let events = run(&runner, input());
        finished(&events).usage.as_ref().unwrap()[0].clone()
    };
    let base = || TokenUsage::default().with_provider("x").with_model("m");

    // A record that only reported a total keeps its tokens in the merge.
    let usage = merged(vec![
        base().with_total_tokens(100),
        base().with_input_tokens(10).with_output_tokens(5),
    ]);
    assert_eq!(
        (usage.input_tokens, usage.output_tokens, usage.total_tokens),
        (None, None, Some(115))
    );

    // A count one record did not report is unknown, not zero.
    let usage = merged(vec![
        base().with_input_tokens(10).with_output_tokens(5),
        base().with_input_tokens(3),
    ]);
    assert_eq!(
        (usage.input_tokens, usage.output_tokens, usage.total_tokens),
        (Some(13), None, None)
    );
}

/// covers: LIFE-34
#[test]
fn run_usage_covers_its_subagents() {
    let runner = Runner::new(agent(|ctx| async move {
        let sub = ctx.emitter().start_subagent("helper").await?;
        // The subagent's model call, recorded through the subagent's own
        // emitter: it is the run's.
        sub.emitter().record_usage(
            TokenUsageBuilder::new("anthropic", "small")
                .input(10, true)
                .output(2, true)
                .build(),
        );
        sub.finish(None, None).await?;
        // The run's own calls, to the same model and to another.
        ctx.emitter().record_usage(
            TokenUsageBuilder::new("anthropic", "small")
                .input(5, true)
                .output(1, true)
                .build(),
        );
        ctx.emitter().record_usage(
            TokenUsageBuilder::new("anthropic", "large")
                .input(100, true)
                .output(20, true)
                .build(),
        );
        Ok(RunOutcome::success())
    }));
    let events = run(&runner, input());
    let usage = finished(&events).usage.clone().unwrap();
    assert_eq!(usage.len(), 2);
    assert_eq!(usage[0].model.as_deref(), Some("small"));
    // 10+2 from the subagent, 5+1 from the run itself.
    assert_eq!(usage[0].total_tokens, Some(18));
    assert_eq!(usage[1].total_tokens, Some(120));
    for event in &events {
        if let Event::SubagentStarted(_) | Event::SubagentFinished(_) = event {
            let wire = serde_json::to_value(event).unwrap();
            assert!(wire.get("usage").is_none());
        }
    }
}

/// covers: LIFE-36
#[test]
fn a_resuming_run_reports_only_its_own_calls() {
    let runner = Runner::new(agent(|ctx| async move {
        if ctx.resume().is_empty() {
            ctx.emitter().record_usage(
                TokenUsageBuilder::new("x", "m")
                    .input(100, true)
                    .output(10, true)
                    .build(),
            );
            Ok(RunOutcome::interrupt([
                agui_sdk::interrupt::Interrupt::new("i", "ask"),
            ]))
        } else {
            ctx.emitter().record_usage(
                TokenUsageBuilder::new("x", "m")
                    .input(3, true)
                    .output(1, true)
                    .build(),
            );
            Ok(RunOutcome::success())
        }
    }));
    run(&runner, input());
    let resumed = run(
        &runner,
        input().with_resume([agui_sdk::interrupt::ResumeEntry::resolved(
            "i",
            serde_json::json!(true),
        )]),
    );
    assert_eq!(
        finished(&resumed).usage.as_ref().unwrap()[0].total_tokens,
        Some(4)
    );
}
