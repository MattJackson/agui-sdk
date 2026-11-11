//! Allocation budget of the reducer's hot path: a text content delta costs an
//! O(1) lookup and an in-place append. When the application drops each
//! update before the next event, the reducer moves the next delta into the
//! previous one's `Arc<Event>`, so steady-state deltas allocate nothing but
//! amortized `String` growth (API.md §8).
//!
//! Allocations are counted on this test's thread only
//! (`agui_test_support::alloc`), so the harness's own threads cannot land in
//! the window.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;

use agui_sdk::diagnostics::{NullSink, Reporter};
use agui_sdk::pipeline::reduce::Reducer;
use agui_sdk::Event;
use agui_test_support::alloc::{self, CountingAllocator};
use serde_json::json;

#[global_allocator]
static GLOBAL: CountingAllocator = CountingAllocator;

fn event(v: serde_json::Value) -> Event {
    serde_json::from_value(v).unwrap()
}

#[test]
fn text_deltas_allocate_only_for_string_growth() {
    const DELTAS: usize = 100_000;
    const DELTA: &str = "token ";
    let mut reducer = Reducer::new();
    let mut reporter = Reporter::new(Arc::new(NullSink), 16);
    for e in [
        json!({"type": "RUN_STARTED", "threadId": "t", "runId": "r"}),
        json!({"type": "TEXT_MESSAGE_START", "messageId": "m1"}),
    ] {
        reducer.apply(event(e), &mut reporter);
    }
    let deltas: Vec<Event> = (0..DELTAS)
        .map(|_| event(json!({"type": "TEXT_MESSAGE_CONTENT", "messageId": "m1", "delta": DELTA})))
        .collect();

    assert!(
        alloc::installed(),
        "the counting allocator is not installed"
    );
    let ((), stats) = alloc::measure(|| {
        for e in deltas {
            let processed = reducer.apply(e, &mut reporter);
            debug_assert!(processed.changes.message.is_some());
        }
    });

    let total_bytes = DELTAS * DELTA.len();
    let growth_bound = (usize::BITS - total_bytes.leading_zeros()) as usize; // ceil-ish log2
                                                                             // Amortized growth of the message text (its first allocation and its
                                                                             // reallocations), plus the first delta's `Arc` (nothing to recycle yet).
    let growth = stats.total();
    assert!(
        growth <= growth_bound as u64 + 1,
        "{} allocations + {} reallocations for {DELTAS} deltas; allowed 1 Arc + log2({total_bytes}) = {}",
        stats.allocations,
        stats.reallocations,
        growth_bound + 1
    );
    let content = serde_json::to_value(reducer.messages()).unwrap()[0]["content"]
        .as_str()
        .unwrap()
        .len();
    assert_eq!(content, total_bytes);
}

/// `TOOL_CALL_END` of a call the run started records the call without keeping
/// the END event's own copy of its id, so a long id is resident once (the
/// START's), not twice.
#[test]
fn tool_call_end_keeps_no_second_id_copy() {
    const MIB: i64 = 1024 * 1024;
    let id = "c".repeat(1024 * 1024);
    let frames: Vec<String> = [
        json!({"type": "RUN_STARTED", "threadId": "t", "runId": "r"}),
        json!({"type": "TOOL_CALL_START", "toolCallId": id, "toolCallName": "f"}),
        json!({"type": "TOOL_CALL_END", "toolCallId": id}),
        json!({"type": "MESSAGES_SNAPSHOT", "messages": []}),
    ]
    .iter()
    .map(ToString::to_string)
    .collect();
    let mut reporter = Reporter::new(Arc::new(NullSink), 16);
    assert!(alloc::installed());
    let (reducer, stats) = alloc::measure(|| {
        let mut reducer = Reducer::new();
        for f in &frames {
            let e: Event = serde_json::from_str(f).unwrap();
            drop(reducer.apply(e, &mut reporter));
        }
        reducer
    });
    assert!(reducer.messages().is_empty());
    assert!(
        stats.live() < MIB * 3 / 2,
        "{} bytes retained for one 1 MiB id",
        stats.live()
    );
}
