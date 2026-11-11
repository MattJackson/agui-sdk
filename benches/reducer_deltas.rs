//! 100k `TEXT_MESSAGE_CONTENT` deltas into one message through the reducer
//! (TEST-PLAN §9.1). Budgets (API.md §8):
//!
//! * `one_message`: the application drops each update before the next
//!   event, so the reducer recycles the event's `Arc` and the only
//!   allocations are the message text's amortized growth: at most
//!   log2(total bytes) plus a small constant for the message itself, and 0
//!   per delta.
//! * `one_message_held`: the application keeps every update, so each event
//!   costs exactly one `Arc<Event>` on top of the same growth.

mod support;

use std::sync::Arc;

use agui_sdk::diagnostics::{NullSink, Reporter};
use agui_sdk::pipeline::reduce::Reducer;
use agui_sdk::Event;
use agui_test_support::alloc::AllocStats;
use serde_json::json;
use support::{Bench, CountingAllocator, Work};

#[global_allocator]
static ALLOC: CountingAllocator = CountingAllocator;

/// The ceiling: `per_event` allocations per delta, plus log2 growth.
fn ceiling(per_event: u64) -> impl Fn(&Work, &AllocStats) -> Result<(), String> {
    move |w: &Work, a: &AllocStats| {
        let growth = (w.bytes.max(2) as f64).log2().ceil() as u64 + 16;
        let limit = per_event * w.events + growth;
        if a.total() <= limit {
            Ok(())
        } else {
            Err(format!(
                "{} allocations > {per_event} × {} events + log2({}) + 16 = {limit}",
                a.total(),
                w.events,
                w.bytes
            ))
        }
    }
}

fn parse(v: serde_json::Value) -> Event {
    serde_json::from_value(v).unwrap_or_else(|e| panic!("{e}"))
}

/// A started message and `deltas` content events for it.
fn inputs(deltas: usize) -> (Event, Vec<Event>, u64) {
    let start = parse(json!({"type": "TEXT_MESSAGE_START", "messageId": "m", "role": "assistant"}));
    let content = (0..deltas)
        .map(|i| parse(json!({"type": "TEXT_MESSAGE_CONTENT", "messageId": "m", "delta": format!("tok{} ", i % 10)})))
        .collect();
    (start, content, 5 * deltas as u64)
}

fn main() {
    let mut bench = Bench::new("reducer_deltas");

    let dropped = ceiling(0);
    bench.case(
        "one_message",
        100_000,
        |deltas| {
            let (start, content, bytes) = inputs(deltas);
            // Events are moved into the reducer, so each pass gets fresh copies
            // (built outside the measured region).
            let prepare = move || {
                let mut reducer = Reducer::new();
                let mut reporter = Reporter::new(Arc::new(NullSink), 256);
                reducer.apply(start.clone(), &mut reporter);
                (reducer, reporter, content.clone())
            };
            let run =
                move |(mut reducer, mut reporter, content): (Reducer, Reporter, Vec<Event>)| {
                    let events = content.len() as u64;
                    for e in content {
                        // The update is dropped at the end of the statement.
                        std::hint::black_box(reducer.apply(e, &mut reporter));
                    }
                    Work { events, bytes }
                };
            (prepare, run)
        },
        Some(&dropped),
    );

    let held = ceiling(1);
    bench.case(
        "one_message_held",
        100_000,
        |deltas| {
            let (start, content, bytes) = inputs(deltas);
            let prepare = move || {
                let mut reducer = Reducer::new();
                let mut reporter = Reporter::new(Arc::new(NullSink), 256);
                reducer.apply(start.clone(), &mut reporter);
                // Room for every update, reserved outside the measured region.
                let kept: Vec<Arc<Event>> = Vec::with_capacity(content.len());
                (reducer, reporter, content.clone(), kept)
            };
            let run = move |(mut reducer, mut reporter, content, mut kept): (
                Reducer,
                Reporter,
                Vec<Event>,
                Vec<Arc<Event>>,
            )| {
                let events = content.len() as u64;
                for e in content {
                    kept.push(reducer.apply(e, &mut reporter).event);
                }
                std::hint::black_box(&kept);
                Work { events, bytes }
            };
            (prepare, run)
        },
        Some(&held),
    );

    // A reopen naming a new parent leaves the call where it is: O(1), not
    // O(calls on the old message).
    bench.case(
        "reopen_new_parent",
        20_000,
        |calls| {
            let open = |i: usize, parent: &str| {
                parse(
                    json!({"type": "TOOL_CALL_START", "toolCallId": format!("c{i}"),
                    "toolCallName": "f", "parentMessageId": parent}),
                )
            };
            let first: Vec<Event> = (0..calls).map(|i| open(i, "A")).collect();
            let reopens: Vec<Event> = (0..calls).map(|i| open(i, "B")).collect();
            let prepare = move || {
                let mut reducer = Reducer::new();
                let mut reporter = Reporter::new(Arc::new(NullSink), 256);
                for e in first.clone() {
                    reducer.apply(e, &mut reporter);
                }
                (reducer, reporter, reopens.clone())
            };
            let run =
                move |(mut reducer, mut reporter, reopens): (Reducer, Reporter, Vec<Event>)| {
                    let events = reopens.len() as u64;
                    for e in reopens {
                        std::hint::black_box(reducer.apply(e, &mut reporter));
                    }
                    Work { events, bytes: 0 }
                };
            (prepare, run)
        },
        None,
    );

    bench.finish();
}
