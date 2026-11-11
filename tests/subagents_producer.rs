//! Producer-side subagents (`events/subagents.mdx`): attribution, the
//! invocation lifecycle, ownership of continuations, steps and tool calls.

#![cfg(feature = "server")]
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support_producer;

use agui_sdk::pipeline::verify::ProducerRule;
use agui_sdk::subagent::SubagentStarted;
use serde_json::{json, Value};
use support_producer::{accepts, finished, refuses, refuses_then, started};

fn sub_started(id: &str) -> Value {
    json!({"type": "SUBAGENT_STARTED", "subagentRunId": id, "name": "helper"})
}

fn sub_finished(id: &str) -> Value {
    json!({"type": "SUBAGENT_FINISHED", "subagentRunId": id})
}

/// covers: SUB-01, SUB-13
#[test]
fn a_subagents_events_carry_its_id_and_the_lifecycle_is_reported() {
    accepts(&[
        started("r1"),
        sub_started("s1"),
        json!({"type": "TEXT_MESSAGE_START", "messageId": "m1", "subagentRunId": "s1"}),
        json!({"type": "TEXT_MESSAGE_END", "messageId": "m1", "subagentRunId": "s1"}),
        json!({"type": "TEXT_MESSAGE_START", "messageId": "m2"}),
        json!({"type": "TEXT_MESSAGE_END", "messageId": "m2"}),
        sub_finished("s1"),
        finished("r1"),
    ]);
}

/// covers: SUB-02
#[test]
fn an_absent_attribution_is_never_spelled_null() {
    // `Option` attribution serializes by omission; an empty id is refused.
    let event = agui_sdk::Event::from(agui_sdk::text::TextMessageStart::new("m1"));
    assert!(serde_json::to_value(&event)
        .unwrap()
        .get("subagentRunId")
        .is_none());
    refuses(
        &[
            started("r1"),
            json!({"type": "STEP_STARTED", "stepName": "s", "subagentRunId": ""}),
        ],
        1,
        ProducerRule::InvalidValue,
    );
}

/// covers: SUB-03, SUB-09
#[test]
fn a_finished_invocations_id_is_not_announced_again_in_the_run() {
    let message = refuses(
        &[
            started("r1"),
            sub_started("s1"),
            sub_finished("s1"),
            sub_started("s1"),
        ],
        3,
        ProducerRule::SubagentIdentity,
    );
    assert!(message.contains("SUB-09"), "{message}");
}

/// covers: SUB-04
#[test]
fn a_suspended_invocation_continues_under_its_id_in_a_later_run() {
    accepts(&[
        started("r1"),
        sub_started("s1"),
        json!({"type": "SUBAGENT_FINISHED", "subagentRunId": "s1", "outcome": {"type": "suspended", "interruptIds": ["i1"]}}),
        finished("r1").as_object().cloned().map(|mut o| {
            o.insert("outcome".into(), json!({"type": "interrupt", "interrupts": [{"id": "i1", "reason": "input", "subagentRunId": "s1"}]}));
            Value::Object(o)
        }).unwrap(),
        started("r2"),
        sub_started("s1"),
        sub_finished("s1"),
        finished("r2"),
    ]);
}

/// covers: SUB-06
#[test]
fn lifecycle_events_carry_the_invocation_they_concern() {
    // The id is a required field: it cannot be omitted from the type...
    let parsed: Result<agui_sdk::Event, _> =
        serde_json::from_value(json!({"type": "SUBAGENT_STARTED", "name": "helper"}));
    assert!(parsed.is_err());
    let _ = SubagentStarted::new("s1", "helper");
    // ...and an empty one is refused.
    refuses(
        &[started("r1"), sub_started("")],
        1,
        ProducerRule::InvalidValue,
    );
}

/// covers: SUB-07
#[test]
fn an_announcement_precedes_attribution() {
    refuses(
        &[
            started("r1"),
            json!({"type": "STEP_STARTED", "stepName": "s", "subagentRunId": "s1"}),
            json!({"type": "STEP_FINISHED", "stepName": "s", "subagentRunId": "s1"}),
            sub_started("s1"),
        ],
        3,
        ProducerRule::AttributedBeforeAnnounced,
    );
}

/// covers: SUB-08
#[test]
fn an_active_id_is_not_announced_again() {
    refuses_then(
        &[started("r1"), sub_started("s1"), sub_started("s1")],
        2,
        ProducerRule::SubagentIdentity,
        Some(sub_finished("s1")),
    );
}

/// covers: SUB-10
#[test]
fn every_announced_invocation_ends_before_the_run() {
    refuses(
        &[started("r1"), sub_started("s1"), finished("r1")],
        2,
        ProducerRule::OpenAtFinish,
    );
    accepts(&[
        started("r1"),
        sub_started("s1"),
        sub_started("s2"),
        json!({"type": "SUBAGENT_ERROR", "subagentRunId": "s2", "message": "boom"}),
        sub_finished("s1"),
        finished("r1"),
    ]);
    // A terminal for an invocation that is not active is refused.
    refuses(
        &[started("r1"), sub_finished("s1")],
        1,
        ProducerRule::NotOpen,
    );
}

/// covers: SUB-11
#[test]
fn attribution_without_announcement_is_legal() {
    accepts(&[
        started("r1"),
        json!({"type": "TEXT_MESSAGE_START", "messageId": "m1", "subagentRunId": "anon"}),
        json!({"type": "TEXT_MESSAGE_END", "messageId": "m1", "subagentRunId": "anon"}),
        finished("r1"),
    ]);
}

/// covers: SUB-15
#[test]
fn usage_is_reported_on_the_run_terminal_only() {
    // Subagent events have no usage field; the run's terminal carries it.
    let parsed: agui_sdk::Event = serde_json::from_value(
        json!({"type": "SUBAGENT_FINISHED", "subagentRunId": "s1", "usage": [{"inputTokens": 1}]}),
    )
    .unwrap();
    assert!(serde_json::to_value(&parsed)
        .unwrap()
        .get("usage")
        .is_none());
    accepts(&[
        started("r1"),
        sub_started("s1"),
        sub_finished("s1"),
        json!({"type": "RUN_FINISHED", "threadId": "t-prod", "runId": "r1", "usage": [{"inputTokens": 10, "outputTokens": 5, "totalTokens": 15}]}),
    ]);
}

/// covers: SUB-17
#[test]
fn outcomes_are_described_ones_and_suspension_is_not_success() {
    let unknown: Result<agui_sdk::Event, _> = serde_json::from_value(
        json!({"type": "SUBAGENT_FINISHED", "subagentRunId": "s1", "outcome": {"type": "paused"}}),
    );
    assert!(unknown.is_err());
    refuses(
        &[
            started("r1"),
            sub_started("s1"),
            json!({"type": "SUBAGENT_FINISHED", "subagentRunId": "s1", "outcome": {"type": "suspended", "interruptIds": ["i1"]}}),
            json!({"type": "RUN_FINISHED", "threadId": "t-prod", "runId": "r1", "outcome": {"type": "success"}}),
        ],
        3,
        ProducerRule::Outcome,
    );
    refuses(
        &[
            started("r1"),
            json!({"type": "RUN_FINISHED", "threadId": "t-prod", "runId": "r1", "outcome": {"type": "interrupt", "interrupts": []}}),
        ],
        1,
        ProducerRule::Outcome,
    );
}

/// covers: SUB-18, SUB-19
#[test]
fn continuations_agree_with_the_openers_owner() {
    let message = refuses(
        &[
            started("r1"),
            sub_started("s1"),
            sub_started("s2"),
            json!({"type": "TEXT_MESSAGE_START", "messageId": "m1", "subagentRunId": "s1"}),
            json!({"type": "TEXT_MESSAGE_CONTENT", "messageId": "m1", "delta": "x", "subagentRunId": "s2"}),
        ],
        4,
        ProducerRule::OwnerMismatch,
    );
    assert!(
        message.contains("s1") && message.contains("s2"),
        "{message}"
    );
    refuses(
        &[
            started("r1"),
            json!({"type": "TEXT_MESSAGE_START", "messageId": "m1"}),
            json!({"type": "TEXT_MESSAGE_END", "messageId": "m1", "subagentRunId": "s1"}),
        ],
        2,
        ProducerRule::OwnerMismatch,
    );
}

/// covers: SUB-20
#[test]
fn an_untagged_continuation_continues_whatever_the_opener_owns() {
    accepts(&[
        started("r1"),
        sub_started("s1"),
        json!({"type": "TEXT_MESSAGE_START", "messageId": "m1", "subagentRunId": "s1"}),
        json!({"type": "TEXT_MESSAGE_CONTENT", "messageId": "m1", "delta": "x"}),
        json!({"type": "TEXT_MESSAGE_END", "messageId": "m1"}),
        sub_finished("s1"),
        finished("r1"),
    ]);
}

/// covers: SUB-21
#[test]
fn reasoning_tool_calls_and_activity_follow_the_same_ownership_rule() {
    refuses(
        &[
            started("r1"),
            json!({"type": "REASONING_MESSAGE_START", "messageId": "r1", "role": "reasoning", "subagentRunId": "s1"}),
            json!({"type": "REASONING_MESSAGE_END", "messageId": "r1", "subagentRunId": "s2"}),
        ],
        2,
        ProducerRule::OwnerMismatch,
    );
    refuses(
        &[
            started("r1"),
            json!({"type": "TOOL_CALL_START", "toolCallId": "c1", "toolCallName": "f", "subagentRunId": "s1"}),
            json!({"type": "TOOL_CALL_ARGS", "toolCallId": "c1", "delta": "{}", "subagentRunId": "s2"}),
        ],
        2,
        ProducerRule::OwnerMismatch,
    );
    refuses(
        &[
            started("r1"),
            json!({"type": "ACTIVITY_SNAPSHOT", "messageId": "a1", "activityType": "t", "content": {}, "subagentRunId": "s1"}),
            json!({"type": "ACTIVITY_DELTA", "messageId": "a1", "activityType": "t", "patch": [], "subagentRunId": "s2"}),
        ],
        2,
        ProducerRule::OwnerMismatch,
    );
}

/// covers: SUB-22, SUB-23
#[test]
fn steps_finish_under_the_attribution_they_started_with() {
    accepts(&[
        started("r1"),
        sub_started("s1"),
        json!({"type": "STEP_STARTED", "stepName": "plan"}),
        json!({"type": "STEP_STARTED", "stepName": "plan", "subagentRunId": "s1"}),
        json!({"type": "STEP_FINISHED", "stepName": "plan", "subagentRunId": "s1"}),
        json!({"type": "STEP_FINISHED", "stepName": "plan"}),
        sub_finished("s1"),
        finished("r1"),
    ]);
    refuses(
        &[
            started("r1"),
            sub_started("s1"),
            json!({"type": "STEP_STARTED", "stepName": "plan", "subagentRunId": "s1"}),
            json!({"type": "STEP_FINISHED", "stepName": "plan"}),
        ],
        3,
        ProducerRule::OwnerMismatch,
    );
}

/// covers(server): SUB-26
#[test]
fn a_tool_call_carries_its_messages_attribution() {
    // A tag naming another owner than the parent message's disagrees.
    refuses(
        &[
            started("r1"),
            sub_started("s1"),
            sub_started("s2"),
            json!({"type": "TEXT_MESSAGE_START", "messageId": "m1", "subagentRunId": "s1"}),
            json!({"type": "TOOL_CALL_START", "toolCallId": "c1", "toolCallName": "f", "parentMessageId": "m1", "subagentRunId": "s2"}),
        ],
        4,
        ProducerRule::OwnerMismatch,
    );
    refuses(
        &[
            started("r1"),
            sub_started("s1"),
            json!({"type": "TEXT_MESSAGE_START", "messageId": "m1"}),
            json!({"type": "TOOL_CALL_START", "toolCallId": "c1", "toolCallName": "f", "parentMessageId": "m1", "subagentRunId": "s1"}),
        ],
        3,
        ProducerRule::OwnerMismatch,
    );
    accepts(&[
        started("r1"),
        sub_started("s1"),
        json!({"type": "TEXT_MESSAGE_START", "messageId": "m1", "subagentRunId": "s1"}),
        json!({"type": "TOOL_CALL_START", "toolCallId": "c1", "toolCallName": "f", "parentMessageId": "m1", "subagentRunId": "s1"}),
        json!({"type": "TOOL_CALL_END", "toolCallId": "c1"}),
        json!({"type": "TEXT_MESSAGE_END", "messageId": "m1"}),
        sub_finished("s1"),
        finished("r1"),
    ]);
}

/// covers(server): SUB-26
#[test]
fn an_untagged_tool_call_inherits_its_parent_messages_attribution() {
    // Explicit events, under an open and under a closed subagent message.
    accepts(&[
        started("r1"),
        sub_started("s1"),
        json!({"type": "TEXT_MESSAGE_START", "messageId": "m1", "subagentRunId": "s1"}),
        json!({"type": "TOOL_CALL_START", "toolCallId": "c1", "toolCallName": "f", "parentMessageId": "m1"}),
        json!({"type": "TOOL_CALL_ARGS", "toolCallId": "c1", "delta": "{}", "subagentRunId": "s1"}),
        json!({"type": "TOOL_CALL_END", "toolCallId": "c1", "subagentRunId": "s1"}),
        json!({"type": "TEXT_MESSAGE_END", "messageId": "m1", "subagentRunId": "s1"}),
        json!({"type": "TOOL_CALL_START", "toolCallId": "c2", "toolCallName": "g", "parentMessageId": "m1"}),
        json!({"type": "TOOL_CALL_END", "toolCallId": "c2"}),
        sub_finished("s1"),
        finished("r1"),
    ]);
    // The shorthand opener inherits the same way.
    accepts(&[
        started("r1"),
        sub_started("s1"),
        json!({"type": "TEXT_MESSAGE_START", "messageId": "m1", "subagentRunId": "s1"}),
        json!({"type": "TEXT_MESSAGE_END", "messageId": "m1", "subagentRunId": "s1"}),
        json!({"type": "TOOL_CALL_CHUNK", "toolCallId": "c1", "toolCallName": "f", "parentMessageId": "m1", "delta": "{}"}),
        sub_finished("s1"),
        finished("r1"),
    ]);
    // Inherited attribution is the call's owner: a continuation tagged to
    // another invocation disagrees with it.
    refuses(
        &[
            started("r1"),
            sub_started("s1"),
            sub_started("s2"),
            json!({"type": "TEXT_MESSAGE_START", "messageId": "m1", "subagentRunId": "s1"}),
            json!({"type": "TOOL_CALL_START", "toolCallId": "c1", "toolCallName": "f", "parentMessageId": "m1"}),
            json!({"type": "TOOL_CALL_END", "toolCallId": "c1", "subagentRunId": "s2"}),
        ],
        5,
        ProducerRule::OwnerMismatch,
    );
}

/// covers: SUB-27
#[test]
fn a_subagent_may_delegate_naming_its_spawner() {
    accepts(&[
        started("r1"),
        sub_started("s1"),
        json!({"type": "SUBAGENT_STARTED", "subagentRunId": "s2", "name": "child", "parentSubagentRunId": "s1"}),
        sub_finished("s2"),
        sub_finished("s1"),
        finished("r1"),
    ]);
}

/// covers: SUB-28
#[test]
fn a_named_parent_was_announced_in_this_run() {
    refuses(
        &[
            started("r1"),
            json!({"type": "SUBAGENT_STARTED", "subagentRunId": "s2", "name": "child", "parentSubagentRunId": "ghost"}),
        ],
        1,
        ProducerRule::SubagentIdentity,
    );
}

/// covers: SUB-29
#[test]
fn parallel_invocations_interleave_and_are_tracked_apart() {
    accepts(&[
        started("r1"),
        sub_started("s1"),
        sub_started("s2"),
        json!({"type": "TEXT_MESSAGE_START", "messageId": "a", "subagentRunId": "s1"}),
        json!({"type": "TEXT_MESSAGE_START", "messageId": "b", "subagentRunId": "s2"}),
        json!({"type": "TEXT_MESSAGE_CHUNK", "messageId": "c", "delta": "x", "subagentRunId": "s2"}),
        json!({"type": "TEXT_MESSAGE_CONTENT", "messageId": "a", "delta": "1", "subagentRunId": "s1"}),
        json!({"type": "TEXT_MESSAGE_END", "messageId": "b", "subagentRunId": "s2"}),
        json!({"type": "TEXT_MESSAGE_END", "messageId": "a", "subagentRunId": "s1"}),
        sub_finished("s2"),
        sub_finished("s1"),
        finished("r1"),
    ]);
}

/// covers: SUB-33
#[test]
fn a_run_does_not_finish_with_an_active_invocation() {
    let message = refuses(
        &[
            started("r1"),
            sub_started("s1"),
            sub_started("s2"),
            sub_finished("s1"),
            finished("r1"),
        ],
        4,
        ProducerRule::OpenAtFinish,
    );
    assert!(message.contains("'s2'"), "{message}");
}

/// covers: SUB-35
#[test]
fn interrupts_may_be_attributed_to_an_invocation() {
    accepts(&[
        started("r1"),
        sub_started("s1"),
        json!({"type": "SUBAGENT_FINISHED", "subagentRunId": "s1", "outcome": {"type": "suspended", "interruptIds": ["i1"]}}),
        json!({"type": "RUN_FINISHED", "threadId": "t-prod", "runId": "r1", "outcome": {"type": "interrupt", "interrupts": [
            {"id": "i1", "reason": "approval", "subagentRunId": "s1"},
            {"id": "i2", "reason": "approval"}
        ]}}),
    ]);
}
