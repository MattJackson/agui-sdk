//! The client's memory bound (docs/DECISIONS.md D14), measured with a
//! counting allocator.
//!
//! - `max_history_bytes` is charged in estimated resident bytes. For each
//!   history shape, the live bytes `L` the stream retains are measured with
//!   the limit off; the estimate must then lie within a band of `L`: the
//!   stream must fail at `lo x L` and stream normally at `1.4 x L`. Where
//!   every allocated byte is resident (maps, chains, messages, calls, parts),
//!   `lo` is 0.9. Large parsed arrays keep capacity slack whose untouched
//!   tail pages are not resident; for them `lo` is 0.7. So does an
//!   `IndexMap` (`preserve_order`) grown member by member, whose entry
//!   vector doubles: a member is charged its share of a full map, so `lo` is
//!   0.75 for that shape with that layout.
//! - The default `PatchLimits::max_nodes` bounds the state a hostile peer can
//!   build with tiny `STATE_DELTA`s.
//!
//! Allocations are counted on this test's thread only, while a measurement
//! is active, so the harness's own threads cannot land in the window. If a
//! dependency bump changes a layout, this fails loudly instead of the
//! documented figure silently becoming false.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;

use agui_sdk::diagnostics::{NullSink, Reporter};
use agui_sdk::pipeline::{Limits, PipelineConfig, Processor};
use agui_sdk::ErrorCode;
use agui_test_support::alloc::{self, CountingAllocator};
use serde_json::{json, Value};

#[global_allocator]
static GLOBAL: CountingAllocator = CountingAllocator;

const MIB: usize = 1024 * 1024;

/// The documented bound on patched state at the default ceiling (D14):
/// about 124 MiB allocated, so the bytes requested stay under it.
const STATE_BOUND: usize = 124 * MIB;

fn frame(v: Value) -> Vec<u8> {
    serde_json::to_vec(&v).unwrap()
}

/// Pushes `frames` after a `RUN_STARTED` with `limits`; returns the error
/// code the stream ended with, if any, and the bytes it retained.
fn stream(frames: &[Vec<u8>], limits: Limits) -> (Option<ErrorCode>, i64) {
    let mut config = PipelineConfig::default();
    config.limits = limits;
    let mut reporter = Reporter::new(Arc::new(NullSink), 16);
    let mut out = Vec::new();
    let start = frame(json!({"type": "RUN_STARTED", "threadId": "t", "runId": "r"}));
    let ((code, processor), stats) = alloc::measure(|| {
        let mut processor = Processor::new(config);
        let mut code = None;
        for f in std::iter::once(&start).chain(frames) {
            if let Err(e) = processor.push_json(f, &mut reporter, &mut out) {
                code = Some(e.code());
                break;
            }
            out.clear();
        }
        out = Vec::new();
        (code, processor)
    });
    drop(processor);
    (code, stats.live())
}

fn with_history_limit(bytes: usize) -> Limits {
    let mut limits = Limits::default();
    limits.max_history_bytes = bytes;
    limits
}

/// The estimate of `frames` lies between `lo` and 1.4 times what they
/// retain.
fn assert_band(name: &str, frames: &[Vec<u8>], lo: f64) {
    assert!(
        alloc::installed(),
        "the counting allocator must be installed"
    );
    let (code, live) = stream(frames, with_history_limit(usize::MAX));
    assert_eq!(code, None, "{name}: streams with the limit off");
    let live = usize::try_from(live).unwrap();
    assert!(live > 4 * MIB, "{name}: a measurable shape ({live} bytes)");
    let at = |factor: f64| (live as f64 * factor) as usize;
    let (low, _) = stream(frames, with_history_limit(at(lo)));
    assert_eq!(
        low,
        Some(ErrorCode::LimitExceeded),
        "{name}: estimated below {lo} x the {live} bytes retained"
    );
    let (high, _) = stream(frames, with_history_limit(at(1.4)));
    assert_eq!(
        high, None,
        "{name}: estimated above 1.4 x the {live} bytes retained"
    );
}

fn activity(id: usize, content: &Value) -> Vec<u8> {
    frame(json!({
        "type": "ACTIVITY_SNAPSHOT", "messageId": format!("a{id}"),
        "activityType": "t", "content": content,
    }))
}

#[test]
fn zeros_array_content() {
    let content = json!({"a": vec![0; 100_000]});
    let frames: Vec<_> = (0..4).map(|i| activity(i, &content)).collect();
    assert_band("zeros", &frames, 0.7);
}

#[test]
fn short_key_object_content() {
    let members: serde_json::Map<String, Value> =
        (0..20_000).map(|k| (format!("k{k}"), json!(0))).collect();
    let content = Value::Object(members);
    let frames: Vec<_> = (0..4).map(|i| activity(i, &content)).collect();
    assert_band("short keys", &frames, 0.9);
}

#[test]
fn long_key_and_string_content() {
    let members: serde_json::Map<String, Value> = (0..10_000)
        .map(|k| (format!("{k:063}"), json!(format!("{k:>63}"))))
        .collect();
    let content = Value::Object(members);
    let frames: Vec<_> = (0..4).map(|i| activity(i, &content)).collect();
    assert_band("63-byte keys and strings", &frames, 0.9);
}

/// Member names in a wire order that leaves `std`'s B-tree leaves at their
/// minimum fill: each time the last leaf is full, the next key lands just
/// right of its middle, so the split leaves five entries behind that later
/// keys never reach.
fn sparse_fill_keys(count: usize) -> Vec<u64> {
    let mut keys = Vec::with_capacity(count);
    let mut leaf: Vec<u64> = Vec::new();
    let mut next = 0u64;
    while keys.len() < count {
        let key = if leaf.len() == 11 {
            let middle = (leaf[5] + leaf[6]) / 2;
            leaf.drain(..6);
            leaf.insert(0, middle);
            middle
        } else {
            next += 1 << 20;
            leaf.push(next);
            next
        };
        keys.push(key);
    }
    keys
}

#[test]
fn object_members_in_adversarial_order() {
    // Built by hand: a `serde_json::Map` would serialize in sorted order.
    let members: Vec<String> = sparse_fill_keys(20_000)
        .iter()
        .map(|k| format!("\"{k:016}\":0"))
        .collect();
    let content = format!("{{{}}}", members.join(","));
    let frames: Vec<_> = (0..4)
        .map(|i| {
            format!(
                r#"{{"type":"ACTIVITY_SNAPSHOT","messageId":"a{i}","activityType":"t","content":{content}}}"#
            )
            .into_bytes()
        })
        .collect();
    assert_band("adversarial member order", &frames, 0.9);
}

/// `{"a":{"a":…{"a":0}}}`, `depth` objects deep.
fn chain(depth: usize) -> Value {
    (0..depth).fold(json!(0), |inner, _| json!({ "a": inner }))
}

#[test]
fn single_member_object_chains() {
    let content = json!({"c": vec![chain(100); 100]});
    let frames: Vec<_> = (0..2).map(|i| activity(i, &content)).collect();
    assert_band("single-member chains", &frames, 0.9);
}

#[test]
fn empty_text_messages() {
    let mut frames = Vec::new();
    for i in 0..20_000 {
        let id = format!("m{i}");
        frames.push(frame(
            json!({"type": "TEXT_MESSAGE_START", "messageId": id, "role": "assistant"}),
        ));
        frames.push(frame(json!({"type": "TEXT_MESSAGE_END", "messageId": id})));
    }
    assert_band("empty text messages", &frames, 0.9);
}

/// Text streamed in deltas: the buffer grows by doubling, and
/// `TEXT_MESSAGE_END` releases the slack, so what the history holds is the
/// text it is charged for. Without the release the estimate is about 0.83 of
/// what is retained.
#[test]
fn streamed_text_messages() {
    let mut frames = Vec::new();
    for i in 0..20_000 {
        let id = format!("m{i}");
        frames.push(frame(
            json!({"type": "TEXT_MESSAGE_START", "messageId": id, "role": "assistant"}),
        ));
        for _ in 0..10 {
            frames.push(frame(
                json!({"type": "TEXT_MESSAGE_CONTENT", "messageId": id, "delta": "abcdefghijklmnopqrst"}),
            ));
        }
        frames.push(frame(json!({"type": "TEXT_MESSAGE_END", "messageId": id})));
    }
    assert_band("streamed text messages", &frames, 0.9);
}

#[test]
fn tool_calls_under_one_parent() {
    let mut frames = vec![frame(
        json!({"type": "TEXT_MESSAGE_START", "messageId": "p", "role": "assistant"}),
    )];
    for i in 0..20_000 {
        let id = format!("c{i}");
        frames.push(frame(json!({
            "type": "TOOL_CALL_START", "toolCallId": id, "toolCallName": "f",
            "parentMessageId": "p",
        })));
        frames.push(frame(json!({"type": "TOOL_CALL_END", "toolCallId": id})));
    }
    assert_band("tool calls", &frames, 0.9);
}

#[test]
fn user_messages_with_text_parts() {
    let parts = vec![json!({"type": "text", "text": ""}); 1_000];
    let messages: Vec<Value> = (0..40)
        .map(|i| json!({"id": format!("u{i}"), "role": "user", "content": parts}))
        .collect();
    let frames = vec![frame(
        json!({"type": "MESSAGES_SNAPSHOT", "messages": messages}),
    )];
    assert_band("text parts", &frames, 0.9);
}

#[test]
fn object_grown_by_add_deltas() {
    let mut frames = vec![activity(0, &json!({}))];
    for d in 0..400 {
        let patch: Vec<Value> = (0..100)
            .map(|k| json!({"op": "add", "path": format!("/k{d}_{k}"), "value": 0}))
            .collect();
        frames.push(frame(json!({
            "type": "ACTIVITY_DELTA", "messageId": "a0", "activityType": "t", "patch": patch,
        })));
    }
    let indexed = std::mem::size_of::<serde_json::Map<String, Value>>()
        != std::mem::size_of::<std::collections::BTreeMap<String, Value>>();
    let lo = if indexed { 0.75 } else { 0.9 };
    assert_band("object grown by add deltas", &frames, lo);
}

/// Security audit S2: one in-limits snapshot, then ~70-byte `copy` deltas
/// until the node ceiling refuses one. Under the default limits the state
/// built that way stays within the documented bound (D14).
#[test]
fn default_patch_ceiling_bounds_resident_state() {
    assert!(alloc::installed());
    let members: serde_json::Map<String, Value> =
        (0..50_000).map(|k| (format!("k{k}"), json!(0))).collect();
    let mut frames = vec![frame(
        json!({"type": "STATE_SNAPSHOT", "snapshot": {"a": Value::Object(members)}}),
    )];
    for k in 0..100 {
        frames.push(frame(json!({
            "type": "STATE_DELTA",
            "delta": [{"op": "copy", "from": "/a", "path": format!("/c{k}")}],
        })));
    }
    let (code, live) = stream(&frames, Limits::default());
    assert_eq!(code, Some(ErrorCode::LimitExceeded), "the ceiling ends it");
    let live = usize::try_from(live).unwrap();
    assert!(live <= STATE_BOUND, "{} MiB of state", live / MIB);
}

/// The worst shape per node unit (single-member objects with 63-byte keys,
/// which cost no extra unit: about 690 bytes requested each) still stays
/// within the bound at the default ceiling. The snapshot is sized so that
/// four copies of it fill the ceiling.
#[test]
fn default_patch_ceiling_bounds_the_worst_shape() {
    assert!(alloc::installed());
    let key = "k".repeat(63);
    let chain = (0..100).fold(json!(0), |inner, _| json!({ key.as_str(): inner }));
    let ceiling = Limits::default().patch.max_nodes;
    let chains = (ceiling / 4 - 1) / 101;
    let snapshot = json!({"a": vec![chain; chains]});
    let mut frames = vec![frame(
        json!({"type": "STATE_SNAPSHOT", "snapshot": snapshot}),
    )];
    for k in 0..100 {
        frames.push(frame(json!({
            "type": "STATE_DELTA",
            "delta": [{"op": "copy", "from": "/a", "path": format!("/c{k}")}],
        })));
    }
    let (code, live) = stream(&frames, Limits::default());
    assert_eq!(code, Some(ErrorCode::LimitExceeded), "the ceiling ends it");
    let live = usize::try_from(live).unwrap();
    assert!(live <= STATE_BOUND, "{} MiB of state", live / MIB);
}

/// The ids the pipeline keeps to check and track a stream are charged (D25),
/// whether or not a message holds them.
#[test]
fn open_steps() {
    let frames: Vec<_> = (0..40_000)
        .map(|i| frame(json!({"type": "STEP_STARTED", "stepName": format!("step-{i:022}")})))
        .collect();
    assert_band("open steps", &frames, 0.9);
}

#[test]
fn reasoning_spans() {
    let mut frames = Vec::new();
    for i in 0..60_000 {
        let id = format!("reasoning-{i:06}");
        frames.push(frame(json!({"type": "REASONING_START", "messageId": id})));
        frames.push(frame(json!({"type": "REASONING_END", "messageId": id})));
    }
    assert_band("reasoning spans", &frames, 0.9);
}

#[test]
fn finished_subagents() {
    let mut frames = Vec::new();
    for i in 0..100_000 {
        let id = format!("subagent-{i:07}");
        frames.push(frame(
            json!({"type": "SUBAGENT_STARTED", "subagentRunId": id, "name": "n"}),
        ));
        frames.push(frame(
            json!({"type": "SUBAGENT_FINISHED", "subagentRunId": id}),
        ));
    }
    assert_band("finished subagents", &frames, 0.9);
}

/// The snapshot runs every round, so the history charge never builds up:
/// what stays is the bookkeeping.
#[test]
fn text_messages_dropped_one_by_one() {
    let mut frames = Vec::new();
    for i in 0..40_000 {
        let id = format!("m{i}");
        frames.push(frame(
            json!({"type": "TEXT_MESSAGE_START", "messageId": id, "role": "assistant"}),
        ));
        frames.push(frame(json!({"type": "TEXT_MESSAGE_END", "messageId": id})));
        frames.push(frame(json!({"type": "MESSAGES_SNAPSHOT", "messages": []})));
    }
    assert_band("text messages dropped one by one", &frames, 0.9);
}

#[test]
fn tool_calls_dropped_one_by_one() {
    let mut frames = Vec::new();
    for i in 0..30_000 {
        let id = format!("c{i}");
        frames.push(frame(
            json!({"type": "TOOL_CALL_START", "toolCallId": id, "toolCallName": "f"}),
        ));
        frames.push(frame(json!({"type": "TOOL_CALL_END", "toolCallId": id})));
        frames.push(frame(json!({"type": "MESSAGES_SNAPSHOT", "messages": []})));
    }
    assert_band("tool calls dropped one by one", &frames, 0.9);
}

/// An array grown by one delta and emptied element by element gives its
/// memory back: removals shrink a sparse array, so repeating the cycle under
/// new keys cannot hold memory the node ceiling no longer counts (D14).
#[test]
fn arrays_emptied_by_remove_deltas_release_memory() {
    assert!(alloc::installed());
    const LEN: usize = 100_000;
    let mut frames = vec![frame(json!({"type": "STATE_SNAPSHOT", "snapshot": {}}))];
    for k in 0..8 {
        frames.push(frame(json!({
            "type": "STATE_DELTA",
            "delta": [{"op": "add", "path": format!("/a{k}"), "value": vec![0; LEN]}],
        })));
        for chunk in (0..LEN).rev().collect::<Vec<_>>().chunks(10_000) {
            let removes: Vec<Value> = chunk
                .iter()
                .map(|i| json!({"op": "remove", "path": format!("/a{k}/{i}")}))
                .collect();
            frames.push(frame(json!({"type": "STATE_DELTA", "delta": removes})));
        }
    }
    let (code, live) = stream(&frames, Limits::default());
    assert_eq!(code, None, "every delta applies");
    let live = usize::try_from(live).unwrap();
    assert!(live < 2 * MIB, "{} KiB stay resident", live / 1024);
}

/// The same for objects: under `preserve-order` an `IndexMap` keeps its
/// capacity on removal unless the applier rebuilds it.
#[test]
fn objects_emptied_by_remove_deltas_release_memory() {
    assert!(alloc::installed());
    const LEN: usize = 4_096;
    let members: serde_json::Map<String, Value> =
        (0..LEN).map(|k| (format!("k{k}"), json!(0))).collect();
    let members = Value::Object(members);
    let mut frames = vec![frame(json!({"type": "STATE_SNAPSHOT", "snapshot": {}}))];
    for k in 0..16 {
        frames.push(frame(json!({
            "type": "STATE_DELTA",
            "delta": [{"op": "add", "path": format!("/o{k}"), "value": members}],
        })));
        for chunk in (0..LEN).rev().collect::<Vec<_>>().chunks(1_000) {
            let removes: Vec<Value> = chunk
                .iter()
                .map(|i| json!({"op": "remove", "path": format!("/o{k}/k{i}")}))
                .collect();
            frames.push(frame(json!({"type": "STATE_DELTA", "delta": removes})));
        }
    }
    let (code, live) = stream(&frames, Limits::default());
    assert_eq!(code, None, "every delta applies");
    let live = usize::try_from(live).unwrap();
    assert!(live < 2 * MIB, "{} KiB stay resident", live / 1024);
}
