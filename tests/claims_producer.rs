//! Producer-side tests behind compliance.toml reasons that the per-area
//! files did not already prove: what the runner's emit helpers put on the
//! wire, and a few verifier verdicts on MAY rows.

#![cfg(feature = "server")]
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support_producer;
mod support_server;

use agui_sdk::event::Event;
use agui_sdk::interrupt::Interrupt;
use agui_sdk::lifecycle::RunFinishedOutcome;
use agui_sdk::pipeline::verify::ProducerRule;
use agui_sdk::server::{RunOutcome, Runner};
use agui_sdk::subagent::SubagentFinishedOutcome;
use serde_json::json;
use support_producer::{accepts, finished, refuses, started};
use support_server::{agent, assert_one_valid_run, input, run};

/// The verifier over the runner's own output, as wire JSON.
fn wire(events: &[Event]) -> Vec<serde_json::Value> {
    events
        .iter()
        .map(|e| serde_json::to_value(e).unwrap())
        .collect()
}

/// covers: BASIC-17
#[test]
fn the_tool_call_id_ties_the_call_its_result_and_its_interrupt() {
    let runner = Runner::new(agent(|ctx| async move {
        let e = ctx.emitter();
        let looked_up = e.start_tool_call("lookup", None).await?.end().await?;
        e.tool_result(looked_up, "found").await?;
        let risky = e.start_tool_call("delete_file", None).await?.end().await?;
        Ok(RunOutcome::interrupt([Interrupt::new(
            "approve-delete",
            "tool_approval",
        )
        .with_tool_call_id(risky)]))
    }));
    let events = run(&runner, input());
    assert_one_valid_run(&events);
    let calls: Vec<_> = events
        .iter()
        .filter_map(|e| match e {
            Event::ToolCallStart(s) => Some(s.tool_call_id.clone()),
            _ => None,
        })
        .collect();
    let result = events
        .iter()
        .find_map(|e| match e {
            Event::ToolCallResult(r) => Some(r.tool_call_id.clone()),
            _ => None,
        })
        .unwrap();
    assert_eq!(result, calls[0]);
    let Some(RunFinishedOutcome::Interrupt { interrupts, .. }) =
        support_server::finished(&events).outcome.clone()
    else {
        panic!("interrupt outcome expected")
    };
    assert_eq!(interrupts[0].tool_call_id.as_ref(), Some(&calls[1]));
}

/// covers: LIFE-05
#[test]
fn every_closing_outcome_is_named_and_an_absent_one_is_accepted() {
    for (outcome, kind) in [
        (RunOutcome::success(), "success"),
        (
            RunOutcome::interrupt([Interrupt::new("i1", "approval")]),
            "interrupt",
        ),
        (RunOutcome::cancelled(), "cancelled"),
    ] {
        let runner = Runner::new(agent(move |_ctx| {
            let outcome = outcome.clone();
            async move { Ok(outcome) }
        }));
        let events = wire(&run(&runner, input()));
        let last = events.last().unwrap();
        assert_eq!(last["type"], "RUN_FINISHED");
        assert_eq!(last["outcome"]["type"], kind);
    }
    // An absent outcome is a success the verifier accepts as it stands.
    accepts(&[started("r1"), finished("r1")]);
}

/// covers: TEXT-08
#[test]
fn content_after_a_message_ends_is_refused() {
    refuses(
        &[
            started("r1"),
            json!({"type": "TEXT_MESSAGE_START", "messageId": "m1"}),
            json!({"type": "TEXT_MESSAGE_CONTENT", "messageId": "m1", "delta": "a"}),
            json!({"type": "TEXT_MESSAGE_END", "messageId": "m1"}),
            json!({"type": "TEXT_MESSAGE_CONTENT", "messageId": "m1", "delta": "b"}),
        ],
        4,
        ProducerRule::NotOpen,
    );
}

/// covers: STRM-19
#[test]
fn a_continuation_changing_name_or_parent_message_is_refused() {
    refuses(
        &[
            started("r1"),
            json!({"type": "TEXT_MESSAGE_CHUNK", "messageId": "m1", "name": "bot", "delta": "a"}),
            json!({"type": "TEXT_MESSAGE_CHUNK", "messageId": "m1", "name": "other", "delta": "b"}),
        ],
        2,
        ProducerRule::ChunkDiscipline,
    );
    refuses(
        &[
            started("r1"),
            json!({"type": "TEXT_MESSAGE_START", "messageId": "m1"}),
            json!({"type": "TEXT_MESSAGE_START", "messageId": "m2"}),
            json!({"type": "TOOL_CALL_CHUNK", "toolCallId": "c1", "toolCallName": "f", "parentMessageId": "m1"}),
            json!({"type": "TOOL_CALL_CHUNK", "toolCallId": "c1", "parentMessageId": "m2", "delta": "{}"}),
        ],
        4,
        ProducerRule::ChunkDiscipline,
    );
}

/// covers: SUB-20
#[test]
fn untagged_reasoning_tool_and_activity_continuations_keep_their_owner() {
    accepts(&[
        started("r1"),
        json!({"type": "SUBAGENT_STARTED", "subagentRunId": "s1", "name": "helper"}),
        json!({"type": "REASONING_START", "messageId": "sp", "subagentRunId": "s1"}),
        json!({"type": "REASONING_MESSAGE_START", "messageId": "r1", "role": "reasoning", "subagentRunId": "s1"}),
        json!({"type": "REASONING_MESSAGE_CONTENT", "messageId": "r1", "delta": "x"}),
        json!({"type": "REASONING_MESSAGE_END", "messageId": "r1"}),
        json!({"type": "REASONING_END", "messageId": "sp"}),
        json!({"type": "TOOL_CALL_START", "toolCallId": "c1", "toolCallName": "f", "subagentRunId": "s1"}),
        json!({"type": "TOOL_CALL_ARGS", "toolCallId": "c1", "delta": "{}"}),
        json!({"type": "TOOL_CALL_END", "toolCallId": "c1"}),
        json!({"type": "ACTIVITY_SNAPSHOT", "messageId": "a1", "activityType": "PLAN", "content": {"n": 1}, "subagentRunId": "s1"}),
        json!({"type": "ACTIVITY_DELTA", "messageId": "a1", "activityType": "PLAN",
               "patch": [{"op": "replace", "path": "/n", "value": 2}]}),
        json!({"type": "SUBAGENT_FINISHED", "subagentRunId": "s1"}),
        finished("r1"),
    ]);
}

/// covers: SUB-23
#[test]
fn parent_and_subagent_step_emitters_hold_same_named_steps() {
    let runner = Runner::new(agent(|ctx| async move {
        let e = ctx.emitter();
        let outer = e.start_step("plan").await?;
        let sub = e.start_subagent("helper").await?;
        let inner = sub.emitter().start_step("plan").await?;
        outer.finish().await?;
        inner.finish().await?;
        sub.finish(None, None).await?;
        Ok(RunOutcome::success())
    }));
    let events = run(&runner, input());
    assert_one_valid_run(&events);
    let steps: Vec<(String, bool, bool)> = events
        .iter()
        .filter_map(|e| match e {
            Event::StepStarted(s) => Some((
                "start".to_owned(),
                s.step_name == "plan",
                s.subagent_run_id.is_some(),
            )),
            Event::StepFinished(s) => Some((
                "finish".to_owned(),
                s.step_name == "plan",
                s.subagent_run_id.is_some(),
            )),
            _ => None,
        })
        .collect();
    assert_eq!(
        steps,
        [
            ("start".to_owned(), true, false),
            ("start".to_owned(), true, true),
            ("finish".to_owned(), true, false),
            ("finish".to_owned(), true, true),
        ]
    );
}

/// covers: SUB-27
#[test]
fn a_nested_invocation_names_its_spawner() {
    let runner = Runner::new(agent(|ctx| async move {
        let outer = ctx.emitter().start_subagent("planner").await?;
        let inner = outer.start_subagent("searcher").await?;
        inner.emitter().say("found").await?;
        inner.finish(None, None).await?;
        outer.finish(None, None).await?;
        Ok(RunOutcome::success())
    }));
    let events = run(&runner, input());
    assert_one_valid_run(&events);
    let started: Vec<_> = events
        .iter()
        .filter_map(|e| match e {
            Event::SubagentStarted(s) => Some(s.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(started.len(), 2);
    assert_eq!(started[0].parent_subagent_run_id, None);
    assert_eq!(
        started[1].parent_subagent_run_id.as_ref(),
        Some(&started[0].subagent_run_id)
    );
}

/// covers: SUB-35
#[test]
fn a_suspended_invocation_and_attributed_interrupts_are_emitted() {
    let runner = Runner::new(agent(|ctx| async move {
        let sub = ctx.emitter().start_subagent("approver").await?;
        let id = sub.id().clone();
        sub.finish(Some(SubagentFinishedOutcome::suspended([])), None)
            .await?;
        Ok(RunOutcome::interrupt([
            Interrupt::new("i1", "approval").with_subagent_run_id(id),
            Interrupt::new("i2", "input"),
        ]))
    }));
    let events = run(&runner, input());
    assert_one_valid_run(&events);
    let wire = wire(&events);
    let sub = wire
        .iter()
        .find(|e| e["type"] == "SUBAGENT_FINISHED")
        .unwrap();
    assert_eq!(sub["outcome"]["type"], "suspended");
    let interrupts = &wire.last().unwrap()["outcome"]["interrupts"];
    assert_eq!(interrupts[0]["subagentRunId"], sub["subagentRunId"]);
    assert!(interrupts[1].get("subagentRunId").is_none());

    // A suspended outcome naming no interrupt ids at all is accepted too.
    accepts(&[
        started("r1"),
        json!({"type": "SUBAGENT_STARTED", "subagentRunId": "s1", "name": "n"}),
        json!({"type": "SUBAGENT_FINISHED", "subagentRunId": "s1", "outcome": {"type": "suspended"}}),
        json!({"type": "RUN_FINISHED", "threadId": "t-prod", "runId": "r1", "outcome": {"type": "interrupt", "interrupts": [
            {"id": "i1", "reason": "approval", "subagentRunId": "s1"}
        ]}}),
    ]);
}

/// covers: PASS-01
#[test]
fn raw_leaves_a_chunk_stream_open_and_custom_ends_it() {
    accepts(&[
        started("r1"),
        json!({"type": "TEXT_MESSAGE_CHUNK", "messageId": "m1", "delta": "a"}),
        json!({"type": "RAW", "event": {"provider": "x"}}),
        json!({"type": "TEXT_MESSAGE_CHUNK", "delta": "b"}),
        finished("r1"),
    ]);
    refuses(
        &[
            started("r1"),
            json!({"type": "TEXT_MESSAGE_CHUNK", "messageId": "m1", "delta": "a"}),
            json!({"type": "CUSTOM", "name": "acme.note", "value": 1}),
            json!({"type": "TEXT_MESSAGE_CHUNK", "delta": "b"}),
        ],
        3,
        ProducerRule::ChunkDiscipline,
    );
}
