//! Producer-side reasoning (`events/reasoning.mdx`): spans and reasoning
//! messages, chunk openers, encrypted values and restating.

#![cfg(feature = "server")]
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support_producer;

use agui_sdk::pipeline::verify::ProducerRule;
use agui_sdk::reasoning::{ReasoningMessageStart, ReasoningRole};
use serde_json::json;
use support_producer::{accepts, finished, refuses, started};

/// covers: REAS-02
#[test]
fn an_open_span_cannot_open_again() {
    refuses(
        &[
            started("r1"),
            json!({"type": "REASONING_START", "messageId": "span"}),
            json!({"type": "REASONING_START", "messageId": "span"}),
        ],
        2,
        ProducerRule::AlreadyOpen,
    );
}

/// covers: REAS-03
#[test]
fn spans_close_before_the_run_finishes() {
    refuses(
        &[
            started("r1"),
            json!({"type": "REASONING_START", "messageId": "span"}),
            finished("r1"),
        ],
        2,
        ProducerRule::OpenAtFinish,
    );
}

/// covers: REAS-04
#[test]
fn only_an_open_span_can_close() {
    refuses(
        &[
            started("r1"),
            json!({"type": "REASONING_END", "messageId": "span"}),
        ],
        1,
        ProducerRule::NotOpen,
    );
}

/// covers: REAS-05
#[test]
fn a_span_holds_several_messages_and_namespaces_nothing() {
    accepts(&[
        started("r1"),
        json!({"type": "REASONING_START", "messageId": "x"}),
        // The span id is not a message id: a reasoning message may share it.
        json!({"type": "REASONING_MESSAGE_START", "messageId": "x", "role": "reasoning"}),
        json!({"type": "REASONING_MESSAGE_END", "messageId": "x"}),
        json!({"type": "REASONING_MESSAGE_START", "messageId": "y", "role": "reasoning"}),
        json!({"type": "REASONING_MESSAGE_END", "messageId": "y"}),
        json!({"type": "REASONING_END", "messageId": "x"}),
        finished("r1"),
    ]);
}

/// covers: REAS-06
#[test]
fn the_reasoning_role_is_fixed_by_the_type() {
    // The role is a one-value enum: no other role can be built or decoded.
    let start = ReasoningMessageStart::new("r1", ReasoningRole::Reasoning);
    let wire = serde_json::to_value(agui_sdk::Event::from(start)).unwrap();
    assert_eq!(wire["role"], "reasoning");
    let other: Result<agui_sdk::Event, _> = serde_json::from_value(
        json!({"type": "REASONING_MESSAGE_START", "messageId": "r1", "role": "assistant"}),
    );
    assert!(other.is_err());
}

/// covers: REAS-07
#[test]
fn the_first_reasoning_chunk_carries_message_id() {
    refuses(
        &[
            started("r1"),
            json!({"type": "REASONING_MESSAGE_CHUNK", "delta": "x"}),
        ],
        1,
        ProducerRule::ChunkDiscipline,
    );
    accepts(&[
        started("r1"),
        json!({"type": "REASONING_MESSAGE_CHUNK", "messageId": "r1", "delta": "x"}),
        json!({"type": "REASONING_MESSAGE_CHUNK", "delta": "y"}),
        finished("r1"),
    ]);
}

/// covers: REAS-18
#[test]
fn a_snapshot_copy_restates_the_artefact() {
    accepts(&[
        started("r1"),
        json!({"type": "REASONING_MESSAGE_START", "messageId": "rm", "role": "reasoning"}),
        json!({"type": "REASONING_ENCRYPTED_VALUE", "subtype": "message", "entityId": "rm", "encryptedValue": "e1"}),
        json!({"type": "REASONING_MESSAGE_END", "messageId": "rm"}),
        json!({"type": "MESSAGES_SNAPSHOT", "messages": [
            {"id": "rm", "role": "reasoning", "content": "", "encryptedValue": "e1"}
        ]}),
        // Omitting it from a later snapshot withdraws it; that is legal too.
        json!({"type": "MESSAGES_SNAPSHOT", "messages": []}),
        finished("r1"),
    ]);
}

/// covers: REAS-19
#[test]
fn an_encrypted_value_appears_anywhere_and_opens_nothing() {
    accepts(&[
        started("r1"),
        json!({"type": "REASONING_ENCRYPTED_VALUE", "subtype": "tool-call", "entityId": "c1", "encryptedValue": "e"}),
        json!({"type": "TEXT_MESSAGE_CHUNK", "messageId": "m1", "delta": "a"}),
        json!({"type": "REASONING_ENCRYPTED_VALUE", "subtype": "message", "entityId": "m1", "encryptedValue": "e"}),
        // It does not end the chunk stream: the continuation still resolves.
        json!({"type": "TEXT_MESSAGE_CHUNK", "delta": "b"}),
        finished("r1"),
    ]);
}
