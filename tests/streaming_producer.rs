//! Producer-side streaming discipline: item open/continue/close, the two
//! spellings, chunk first-fields and continuations, lanes per owner
//! (`patterns/streaming.mdx`), enforced by `ProducerVerifier` and through the
//! runner's emit path.

#![cfg(feature = "server")]
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support_producer;

use agui_sdk::pipeline::verify::ProducerRule;
use serde_json::json;
use support_producer::{accepts, finished, refuses, refuses_then, started};

/// covers: STRM-01
#[test]
fn an_open_item_cannot_be_opened_again() {
    // The follow-up is the item's own END: it is accepted only if the refused
    // duplicate left the item open.
    for (open, again, end) in [
        (
            json!({"type": "TEXT_MESSAGE_START", "messageId": "m1"}),
            json!({"type": "TEXT_MESSAGE_START", "messageId": "m1"}),
            json!({"type": "TEXT_MESSAGE_END", "messageId": "m1"}),
        ),
        (
            json!({"type": "TOOL_CALL_START", "toolCallId": "c1", "toolCallName": "f"}),
            json!({"type": "TOOL_CALL_START", "toolCallId": "c1", "toolCallName": "f"}),
            json!({"type": "TOOL_CALL_END", "toolCallId": "c1"}),
        ),
        (
            json!({"type": "REASONING_MESSAGE_START", "messageId": "r1", "role": "reasoning"}),
            json!({"type": "REASONING_MESSAGE_START", "messageId": "r1", "role": "reasoning"}),
            json!({"type": "REASONING_MESSAGE_END", "messageId": "r1"}),
        ),
    ] {
        let message = refuses_then(
            &[started("r1"), open, again],
            2,
            ProducerRule::AlreadyOpen,
            Some(end),
        );
        assert!(message.contains("already open"), "{message}");
    }
}

/// covers: STRM-02
#[test]
fn content_and_end_need_an_open_item() {
    for orphan in [
        json!({"type": "TEXT_MESSAGE_CONTENT", "messageId": "m1", "delta": "x"}),
        json!({"type": "TEXT_MESSAGE_END", "messageId": "m1"}),
        json!({"type": "TOOL_CALL_ARGS", "toolCallId": "c1", "delta": "{}"}),
        json!({"type": "TOOL_CALL_END", "toolCallId": "c1"}),
        json!({"type": "REASONING_MESSAGE_CONTENT", "messageId": "r1", "delta": "x"}),
    ] {
        refuses(&[started("r1"), orphan], 1, ProducerRule::NotOpen);
    }
    // Closed is not open either.
    refuses(
        &[
            started("r1"),
            json!({"type": "TEXT_MESSAGE_START", "messageId": "m1"}),
            json!({"type": "TEXT_MESSAGE_END", "messageId": "m1"}),
            json!({"type": "TEXT_MESSAGE_CONTENT", "messageId": "m1", "delta": "late"}),
        ],
        3,
        ProducerRule::NotOpen,
    );
}

/// covers: STRM-03
#[test]
fn every_opened_item_closes_before_run_finished() {
    for open in [
        json!({"type": "TEXT_MESSAGE_START", "messageId": "m1"}),
        json!({"type": "TOOL_CALL_START", "toolCallId": "c1", "toolCallName": "f"}),
        json!({"type": "REASONING_MESSAGE_START", "messageId": "r1", "role": "reasoning"}),
        json!({"type": "STEP_STARTED", "stepName": "s"}),
    ] {
        let message = refuses(
            &[started("r1"), open, finished("r1")],
            2,
            ProducerRule::OpenAtFinish,
        );
        assert!(message.contains("still open"), "{message}");
    }
    // A chunk stream is ended by the terminal itself: nothing is left open.
    accepts(&[
        started("r1"),
        json!({"type": "TEXT_MESSAGE_CHUNK", "messageId": "m1", "delta": "x"}),
        finished("r1"),
    ]);
}

/// covers: STRM-05
#[test]
fn families_interleave_each_with_its_own_discipline() {
    accepts(&[
        started("r1"),
        json!({"type": "STEP_STARTED", "stepName": "plan"}),
        json!({"type": "TEXT_MESSAGE_START", "messageId": "m1"}),
        json!({"type": "REASONING_START", "messageId": "span"}),
        json!({"type": "TOOL_CALL_START", "toolCallId": "c1", "toolCallName": "f", "parentMessageId": "m1"}),
        json!({"type": "TEXT_MESSAGE_CONTENT", "messageId": "m1", "delta": "a"}),
        json!({"type": "REASONING_MESSAGE_START", "messageId": "rm", "role": "reasoning"}),
        json!({"type": "TOOL_CALL_ARGS", "toolCallId": "c1", "delta": "{}"}),
        json!({"type": "TEXT_MESSAGE_START", "messageId": "m2"}),
        json!({"type": "REASONING_MESSAGE_END", "messageId": "rm"}),
        json!({"type": "TOOL_CALL_END", "toolCallId": "c1"}),
        json!({"type": "TEXT_MESSAGE_END", "messageId": "m1"}),
        json!({"type": "REASONING_END", "messageId": "span"}),
        json!({"type": "TEXT_MESSAGE_END", "messageId": "m2"}),
        json!({"type": "STEP_FINISHED", "stepName": "plan"}),
        finished("r1"),
    ]);
}

/// covers: STRM-06
#[test]
fn standalone_events_appear_anywhere_in_an_open_run() {
    accepts(&[
        started("r1"),
        json!({"type": "TEXT_MESSAGE_START", "messageId": "m1"}),
        json!({"type": "STATE_SNAPSHOT", "snapshot": {"n": 1}}),
        json!({"type": "STATE_DELTA", "delta": [{"op": "replace", "path": "/n", "value": 2}]}),
        json!({"type": "MESSAGES_SNAPSHOT", "messages": []}),
        json!({"type": "ACTIVITY_SNAPSHOT", "messageId": "a1", "activityType": "t", "content": {}}),
        json!({"type": "CUSTOM", "name": "c", "value": null}),
        json!({"type": "RAW", "event": {"x": 1}}),
        json!({"type": "REASONING_ENCRYPTED_VALUE", "subtype": "message", "entityId": "m1", "encryptedValue": "e"}),
        json!({"type": "TEXT_MESSAGE_END", "messageId": "m1"}),
        finished("r1"),
    ]);
}

/// covers: STRM-09
#[test]
fn an_item_keeps_one_spelling() {
    // Chunk-opened, continued explicitly.
    refuses(
        &[
            started("r1"),
            json!({"type": "TEXT_MESSAGE_CHUNK", "messageId": "m1", "delta": "a"}),
            json!({"type": "TEXT_MESSAGE_CONTENT", "messageId": "m1", "delta": "b"}),
        ],
        2,
        ProducerRule::ChunkDiscipline,
    );
    // `*_END` of a chunk item is synthesized, never sent.
    refuses(
        &[
            started("r1"),
            json!({"type": "TOOL_CALL_CHUNK", "toolCallId": "c1", "toolCallName": "f"}),
            json!({"type": "TOOL_CALL_END", "toolCallId": "c1"}),
        ],
        2,
        ProducerRule::ChunkDiscipline,
    );
    // Explicitly opened, continued in chunk form.
    let message = refuses(
        &[
            started("r1"),
            json!({"type": "TEXT_MESSAGE_START", "messageId": "m1"}),
            json!({"type": "TEXT_MESSAGE_CHUNK", "messageId": "m1", "delta": "b"}),
        ],
        2,
        ProducerRule::ChunkDiscipline,
    );
    assert!(message.contains("STRM-09"), "{message}");
}

/// covers: STRM-11
#[test]
fn the_first_text_chunk_carries_message_id() {
    refuses(
        &[
            started("r1"),
            json!({"type": "TEXT_MESSAGE_CHUNK", "delta": "x"}),
        ],
        1,
        ProducerRule::ChunkDiscipline,
    );
}

/// covers: STRM-12
#[test]
fn the_first_text_chunk_may_carry_a_role() {
    accepts(&[
        started("r1"),
        json!({"type": "TEXT_MESSAGE_CHUNK", "messageId": "m1", "role": "user", "delta": "x"}),
        json!({"type": "TEXT_MESSAGE_CHUNK", "messageId": "m1", "role": "user", "delta": "y"}),
        json!({"type": "TEXT_MESSAGE_CHUNK", "messageId": "m2", "delta": "absent role is assistant"}),
        json!({"type": "TEXT_MESSAGE_CHUNK", "messageId": "m2", "role": "assistant"}),
        finished("r1"),
    ]);
}

/// covers: STRM-13
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
}

/// covers: STRM-14
#[test]
fn the_first_tool_chunk_carries_id_and_name() {
    let without_name = refuses(
        &[
            started("r1"),
            json!({"type": "TOOL_CALL_CHUNK", "toolCallId": "c1", "delta": "{"}),
        ],
        1,
        ProducerRule::ChunkDiscipline,
    );
    assert!(without_name.contains("toolCallName"), "{without_name}");
    refuses(
        &[
            started("r1"),
            json!({"type": "TOOL_CALL_CHUNK", "toolCallName": "f", "delta": "{"}),
        ],
        1,
        ProducerRule::ChunkDiscipline,
    );
}

/// covers: STRM-15
#[test]
fn later_chunks_omit_opener_fields_and_continue() {
    accepts(&[
        started("r1"),
        json!({"type": "TOOL_CALL_CHUNK", "toolCallId": "c1", "toolCallName": "f", "delta": "{"}),
        json!({"type": "TOOL_CALL_CHUNK", "delta": "\"a\":1"}),
        json!({"type": "TOOL_CALL_CHUNK", "delta": "}"}),
        json!({"type": "REASONING_MESSAGE_CHUNK", "messageId": "r1", "delta": "x"}),
        json!({"type": "REASONING_MESSAGE_CHUNK", "delta": "y"}),
        finished("r1"),
    ]);
}

/// covers: STRM-17
#[test]
fn a_chunk_naming_another_id_does_not_continue_the_open_one() {
    refuses(
        &[
            started("r1"),
            json!({"type": "TOOL_CALL_CHUNK", "toolCallId": "c1", "toolCallName": "f"}),
            json!({"type": "TOOL_CALL_CHUNK", "toolCallId": "c1", "delta": "{"}),
            json!({"type": "TOOL_CALL_CHUNK", "toolCallId": "c2", "delta": "}"}),
        ],
        3,
        ProducerRule::ChunkDiscipline,
    );
    // CUSTOM ends the lane's stream: an id-less chunk then has no referent.
    refuses(
        &[
            started("r1"),
            json!({"type": "TEXT_MESSAGE_CHUNK", "messageId": "m1", "delta": "a"}),
            json!({"type": "CUSTOM", "name": "n", "value": 1}),
            json!({"type": "TEXT_MESSAGE_CHUNK", "delta": "b"}),
        ],
        3,
        ProducerRule::ChunkDiscipline,
    );
}

/// covers: STRM-19
#[test]
fn a_continuation_may_repeat_opener_fields_unchanged_only() {
    accepts(&[
        started("r1"),
        json!({"type": "TEXT_MESSAGE_CHUNK", "messageId": "m1", "role": "assistant", "name": "bot", "delta": "a"}),
        json!({"type": "TEXT_MESSAGE_CHUNK", "messageId": "m1", "role": "assistant", "name": "bot", "delta": "b"}),
        json!({"type": "TOOL_CALL_CHUNK", "toolCallId": "c1", "toolCallName": "f", "parentMessageId": "m1"}),
        json!({"type": "TOOL_CALL_CHUNK", "toolCallId": "c1", "toolCallName": "f", "parentMessageId": "m1", "delta": "{}"}),
        finished("r1"),
    ]);
    refuses(
        &[
            started("r1"),
            json!({"type": "TEXT_MESSAGE_CHUNK", "messageId": "m1", "delta": "a"}),
            json!({"type": "TEXT_MESSAGE_CHUNK", "role": "user", "delta": "b"}),
        ],
        2,
        ProducerRule::ChunkDiscipline,
    );
    refuses(
        &[
            started("r1"),
            json!({"type": "TOOL_CALL_CHUNK", "toolCallId": "c1", "toolCallName": "f"}),
            json!({"type": "TOOL_CALL_CHUNK", "toolCallName": "g", "delta": "{}"}),
        ],
        2,
        ProducerRule::ChunkDiscipline,
    );
}

/// covers: STRM-25
#[test]
fn parallel_subagents_attribute_their_continuations() {
    let subagents = [
        started("r1"),
        json!({"type": "SUBAGENT_STARTED", "subagentRunId": "s1", "name": "a"}),
        json!({"type": "SUBAGENT_STARTED", "subagentRunId": "s2", "name": "b"}),
        json!({"type": "TEXT_MESSAGE_CHUNK", "messageId": "m1", "delta": "a", "subagentRunId": "s1"}),
    ];
    let mut untagged = subagents.to_vec();
    untagged.push(json!({"type": "TEXT_MESSAGE_CHUNK", "delta": "b"}));
    let message = refuses(&untagged, 4, ProducerRule::ChunkDiscipline);
    assert!(message.contains("STRM-25"), "{message}");
    let mut tagged = subagents.to_vec();
    tagged.extend([
        json!({"type": "TEXT_MESSAGE_CHUNK", "delta": "b", "subagentRunId": "s1"}),
        json!({"type": "SUBAGENT_FINISHED", "subagentRunId": "s1"}),
        json!({"type": "SUBAGENT_FINISHED", "subagentRunId": "s2"}),
        finished("r1"),
    ]);
    accepts(&tagged);
}

/// covers: STRM-26
#[test]
fn a_chunk_stream_does_not_outlive_its_subagent() {
    let prefix = [
        started("r1"),
        json!({"type": "SUBAGENT_STARTED", "subagentRunId": "s1", "name": "a"}),
        json!({"type": "TEXT_MESSAGE_CHUNK", "messageId": "m1", "delta": "a", "subagentRunId": "s1"}),
        json!({"type": "SUBAGENT_FINISHED", "subagentRunId": "s1"}),
    ];
    let mut late = prefix.to_vec();
    late.push(json!({"type": "TEXT_MESSAGE_CHUNK", "delta": "late", "subagentRunId": "s1"}));
    let message = refuses(&late, 4, ProducerRule::ChunkDiscipline);
    assert!(message.contains("STRM-26"), "{message}");
    // Nor may an explicit item a subagent opened outlive it.
    refuses(
        &[
            started("r1"),
            json!({"type": "SUBAGENT_STARTED", "subagentRunId": "s1", "name": "a"}),
            json!({"type": "TOOL_CALL_START", "toolCallId": "c1", "toolCallName": "f", "subagentRunId": "s1"}),
            json!({"type": "SUBAGENT_FINISHED", "subagentRunId": "s1"}),
        ],
        3,
        ProducerRule::OpenAtFinish,
    );
}
