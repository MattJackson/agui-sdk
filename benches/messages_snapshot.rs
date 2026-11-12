//! Reconciling a `MESSAGES_SNAPSHOT` of 10k messages against held history.
//! Gate: O(n) time and allocations (linearity), exact budget per message.

mod support;

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;

use agui_sdk::diagnostics::{NullSink, Reporter};
use agui_sdk::pipeline::reduce::Reducer;
use agui_sdk::pipeline::Processed;
use agui_sdk::Event;
use serde_json::json;
use support::{Bench, CountingAllocator, Work};

/// Where a timed `run` parks its reducer (and what it produced) so their
/// O(held) drop happens in the next untimed `prepare`, not in the timed
/// region.
type Parked<T> = Rc<RefCell<Option<T>>>;

#[global_allocator]
static ALLOC: CountingAllocator = CountingAllocator;

fn main() {
    let mut bench = Bench::new("messages_snapshot");
    bench.case("reconcile", 10_000, |n| {
        let messages: Vec<_> = (0..n)
            .map(|i| json!({"id": format!("m-{i}"), "role": if i % 2 == 0 { "user" } else { "assistant" }, "content": format!("message {i}")}))
            .collect();
        let bytes = serde_json::to_vec(&messages).map_or(0, |v| v.len() as u64);
        let held: Event = serde_json::from_value(json!({"type": "MESSAGES_SNAPSHOT", "messages": messages[..n / 2]}))
            .unwrap_or_else(|e| panic!("{e}"));
        let snapshot: Event = serde_json::from_value(json!({"type": "MESSAGES_SNAPSHOT", "messages": messages}))
            .unwrap_or_else(|e| panic!("{e}"));
        let parked: Parked<(Reducer, Reporter, Processed)> = Rc::default();
        let park = Rc::clone(&parked);
        let prepare = move || {
            drop(park.borrow_mut().take());
            let mut reducer = Reducer::new();
            let mut reporter = Reporter::new(Arc::new(NullSink), 256);
            reducer.apply(held.clone(), &mut reporter);
            (reducer, reporter, snapshot.clone())
        };
        let run = move |(mut reducer, mut reporter, snapshot): (Reducer, Reporter, Event)| {
            let update = std::hint::black_box(reducer.apply(snapshot, &mut reporter));
            *parked.borrow_mut() = Some((reducer, reporter, update));
            Work { events: n as u64, bytes }
        };
        (prepare, run)
    }, None);
    // M tiny snapshots against N held client-only messages (M = N): each
    // costs O(snapshot), not O(held).
    bench.case(
        "tiny_snapshots_vs_held_activity",
        10_000,
        |n| {
            let activity: Vec<Event> = (0..n)
                .map(|i| {
                    serde_json::from_value(
                        json!({"type": "ACTIVITY_SNAPSHOT", "messageId": format!("a-{i}"),
                "activityType": "t", "content": {}}),
                    )
                    .unwrap_or_else(|e| panic!("{e}"))
                })
                .collect();
            let tiny: Event =
                serde_json::from_value(json!({"type": "MESSAGES_SNAPSHOT", "messages": []}))
                    .unwrap_or_else(|e| panic!("{e}"));
            let parked: Parked<(Reducer, Reporter)> = Rc::default();
            let park = Rc::clone(&parked);
            let prepare = move || {
                drop(park.borrow_mut().take());
                let mut reducer = Reducer::new();
                let mut reporter = Reporter::new(Arc::new(NullSink), 256);
                for e in activity.clone() {
                    reducer.apply(e, &mut reporter);
                }
                (reducer, reporter, vec![tiny.clone(); n])
            };
            let run =
                move |(mut reducer, mut reporter, snapshots): (Reducer, Reporter, Vec<Event>)| {
                    let events = snapshots.len() as u64;
                    for e in snapshots {
                        std::hint::black_box(reducer.apply(e, &mut reporter));
                    }
                    *parked.borrow_mut() = Some((reducer, reporter));
                    Work { events, bytes: 0 }
                };
            (prepare, run)
        },
        None,
    );
    // Tiny snapshots that swap a held activity into a user message and drop
    // it again (M = N): each pair is bounded by the reconcile work bank, so
    // the stream stays linear (the reducer refuses the drops once it runs dry).
    bench.case(
        "alternating_class_swap_drops",
        10_000,
        |n| {
            let activity: Vec<Event> = (0..n)
                .map(|i| {
                    serde_json::from_value(
                        json!({"type": "ACTIVITY_SNAPSHOT", "messageId": format!("a-{i}"),
                "activityType": "t", "content": {}}),
                    )
                    .unwrap_or_else(|e| panic!("{e}"))
                })
                .collect();
            let frames: Vec<Event> = (0..n)
                .flat_map(|i| {
                    [
                        json!({"type": "MESSAGES_SNAPSHOT", "messages":
                            [{"id": format!("a-{i}"), "role": "user", "content": ""}]}),
                        json!({"type": "MESSAGES_SNAPSHOT", "messages": []}),
                    ]
                })
                .map(|v| serde_json::from_value(v).unwrap_or_else(|e| panic!("{e}")))
                .collect();
            let parked: Parked<(Reducer, Reporter)> = Rc::default();
            let park = Rc::clone(&parked);
            let prepare = move || {
                drop(park.borrow_mut().take());
                let mut reducer = Reducer::new();
                let mut reporter = Reporter::new(Arc::new(NullSink), 256);
                for e in activity.clone() {
                    reducer.apply(e, &mut reporter);
                }
                (reducer, reporter, frames.clone())
            };
            let run =
                move |(mut reducer, mut reporter, frames): (Reducer, Reporter, Vec<Event>)| {
                    let events = frames.len() as u64;
                    for e in frames {
                        std::hint::black_box(reducer.apply(e, &mut reporter));
                    }
                    *parked.borrow_mut() = Some((reducer, reporter));
                    Work { events, bytes: 0 }
                };
            (prepare, run)
        },
        None,
    );
    // Tiny snapshots that move a duplicated call id between its two holders
    // (A, N activities, B): finding the next holder never scans the history.
    bench.case(
        "alternating_duplicate_call_holder",
        10_000,
        |n| {
            let call = json!([{"id": "c", "type": "function",
                "function": {"name": "f", "arguments": ""}}]);
            let held: Vec<Event> = [json!({"type": "MESSAGES_SNAPSHOT", "messages": [
                    {"id": "A", "role": "assistant", "toolCalls": call},
                    {"id": "B", "role": "assistant", "toolCalls": call}]})]
            .into_iter()
            .chain((0..n).map(|i| {
                json!({"type": "ACTIVITY_SNAPSHOT", "messageId": format!("a-{i}"),
                    "activityType": "t", "content": {}})
            }))
            .map(|v| serde_json::from_value(v).unwrap_or_else(|e| panic!("{e}")))
            .collect();
            let frames: Vec<Event> = (0..n)
                .map(|i| {
                    let a = if i % 2 == 0 {
                        json!({"id": "A", "role": "assistant", "content": ""})
                    } else {
                        json!({"id": "A", "role": "assistant", "toolCalls": call})
                    };
                    serde_json::from_value(json!({"type": "MESSAGES_SNAPSHOT", "messages":
                        [a, {"id": "B", "role": "assistant", "toolCalls": call}]}))
                    .unwrap_or_else(|e| panic!("{e}"))
                })
                .collect();
            let parked: Parked<(Reducer, Reporter)> = Rc::default();
            let park = Rc::clone(&parked);
            let prepare = move || {
                drop(park.borrow_mut().take());
                let mut reducer = Reducer::new();
                let mut reporter = Reporter::new(Arc::new(NullSink), 256);
                for e in held.clone() {
                    reducer.apply(e, &mut reporter);
                }
                (reducer, reporter, frames.clone())
            };
            let run =
                move |(mut reducer, mut reporter, frames): (Reducer, Reporter, Vec<Event>)| {
                    let events = frames.len() as u64;
                    for e in frames {
                        std::hint::black_box(reducer.apply(e, &mut reporter));
                    }
                    *parked.borrow_mut() = Some((reducer, reporter));
                    Work { events, bytes: 0 }
                };
            (prepare, run)
        },
        None,
    );
    bench.finish();
}
