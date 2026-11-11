//! Producer-side text messages (`events/text-messages.mdx`): roles and names,
//! reopening, and restating by snapshot, as the producer verifier and the
//! runner's `TextStream` emit them.

#![cfg(feature = "server")]
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support_producer;

use agui_sdk::pipeline::verify::ProducerRule;
use agui_sdk::server::{RunOutcome, Runner};
use agui_sdk::text::TextMessageRole;
use serde_json::json;
use support_producer::{accepts, agent, finished, refuses, started};

/// covers: TEXT-03
#[test]
fn start_may_carry_a_role_and_a_name() {
    accepts(&[
        started("r1"),
        json!({"type": "TEXT_MESSAGE_START", "messageId": "m1"}),
        json!({"type": "TEXT_MESSAGE_END", "messageId": "m1"}),
        json!({"type": "TEXT_MESSAGE_START", "messageId": "m2", "role": "system", "name": "ops"}),
        json!({"type": "TEXT_MESSAGE_END", "messageId": "m2"}),
        finished("r1"),
    ]);
    // Through the runner: a role chosen by the agent reaches the wire.
    let runner = Runner::new(agent(|ctx| async move {
        let text = ctx.emitter().start_text(TextMessageRole::Developer).await?;
        text.push("note").await?;
        text.end().await?;
        Ok(RunOutcome::success())
    }));
    let events = support_producer::runtime().block_on(collect(&runner));
    let start = events
        .iter()
        .find(|e| e["type"] == "TEXT_MESSAGE_START")
        .expect("a start");
    assert_eq!(start["role"], "developer");
}

/// covers: TEXT-05
#[test]
fn a_closed_message_may_reopen_and_append() {
    accepts(&[
        started("r1"),
        json!({"type": "TEXT_MESSAGE_START", "messageId": "m1", "role": "assistant"}),
        json!({"type": "TEXT_MESSAGE_CONTENT", "messageId": "m1", "delta": "a"}),
        json!({"type": "TEXT_MESSAGE_END", "messageId": "m1"}),
        json!({"type": "TEXT_MESSAGE_START", "messageId": "m1", "role": "assistant"}),
        json!({"type": "TEXT_MESSAGE_CONTENT", "messageId": "m1", "delta": "b"}),
        json!({"type": "TEXT_MESSAGE_END", "messageId": "m1"}),
        finished("r1"),
    ]);
}

/// covers: TEXT-06
#[test]
fn a_reopening_start_agrees_with_the_message() {
    let closed = [
        started("r1"),
        json!({"type": "SUBAGENT_STARTED", "subagentRunId": "s1", "name": "a"}),
        json!({"type": "TEXT_MESSAGE_START", "messageId": "m1", "role": "assistant", "name": "bot", "subagentRunId": "s1"}),
        json!({"type": "TEXT_MESSAGE_END", "messageId": "m1"}),
    ];
    for (field, reopen) in [
        (
            "role",
            json!({"type": "TEXT_MESSAGE_START", "messageId": "m1", "role": "user", "name": "bot", "subagentRunId": "s1"}),
        ),
        (
            "name",
            json!({"type": "TEXT_MESSAGE_START", "messageId": "m1", "role": "assistant", "name": "other", "subagentRunId": "s1"}),
        ),
        (
            "owner",
            json!({"type": "TEXT_MESSAGE_START", "messageId": "m1", "role": "assistant", "name": "bot"}),
        ),
    ] {
        let mut stream = closed.to_vec();
        stream.push(reopen);
        let message = refuses(&stream, 4, ProducerRule::OwnerMismatch);
        assert!(message.contains("TEXT-06"), "{field}: {message}");
    }
}

/// covers: TEXT-08
#[test]
fn a_snapshot_restates_closed_messages_by_id() {
    accepts(&[
        started("r1"),
        json!({"type": "TEXT_MESSAGE_START", "messageId": "m1"}),
        json!({"type": "TEXT_MESSAGE_CONTENT", "messageId": "m1", "delta": "draft"}),
        json!({"type": "TEXT_MESSAGE_END", "messageId": "m1"}),
        json!({"type": "MESSAGES_SNAPSHOT", "messages": [
            {"id": "u1", "role": "user", "content": "hi"},
            {"id": "m1", "role": "assistant", "content": "final"}
        ]}),
        finished("r1"),
    ]);
    // A snapshot names each message once.
    refuses(
        &[
            started("r1"),
            json!({"type": "MESSAGES_SNAPSHOT", "messages": [
                {"id": "m1", "role": "assistant", "content": "a"},
                {"id": "m1", "role": "assistant", "content": "b"}
            ]}),
        ],
        1,
        ProducerRule::DuplicateMessageId,
    );
}

async fn collect<A: agui_sdk::server::Agent>(runner: &Runner<A>) -> Vec<serde_json::Value> {
    use agui_sdk::server::input::{admit, InputPolicy};
    let admitted = admit(
        &support_producer::input_json("t1", "r1"),
        &InputPolicy::default(),
    )
    .unwrap();
    let mut stream = runner
        .run(
            admitted,
            agui_sdk::server::CancelToken::new(),
            agui_sdk::server::Extensions::new(),
        )
        .unwrap();
    let mut out = Vec::new();
    while let Some(event) = stream.next().await {
        out.push(serde_json::to_value(&event).unwrap());
    }
    out
}
