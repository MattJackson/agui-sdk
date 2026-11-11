//! Parse and serialize cost of typical events (budgets, TEST-PLAN §9).
//!
//! `cargo bench --bench protocol_types [-- --quick]`. Reports median and p95
//! nanoseconds per event, MB/s, and allocations per event from a counting
//! global allocator; fails if the allocation budget is exceeded. Runs at x1
//! and x8 stream sizes and fails when allocations per event differ between
//! them (the per-event cost must not grow with the stream).

// Test code: a failed unwrap is a failed test.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::alloc::System;
use std::hint::black_box;
use std::time::Instant;

use agui_sdk::event::Event;
use stats_alloc::{Region, StatsAlloc, INSTRUMENTED_SYSTEM};

#[global_allocator]
static GLOBAL: &StatsAlloc<System> = &INSTRUMENTED_SYSTEM;

/// A text-heavy run: what a chat turn looks like on the wire. Each entry is
/// paired with the owned strings its typed form keeps (the parse budget).
const TYPICAL: &[(&str, usize)] = &[
    (
        r#"{"type":"RUN_STARTED","threadId":"thread-1","runId":"run-1"}"#,
        2,
    ),
    (
        r#"{"type":"TEXT_MESSAGE_START","messageId":"msg-1","role":"assistant"}"#,
        1,
    ),
    (
        r#"{"type":"TEXT_MESSAGE_CONTENT","messageId":"msg-1","delta":"Hello, here is the answer you asked for"}"#,
        2,
    ),
    (
        r#"{"type":"TEXT_MESSAGE_CONTENT","messageId":"msg-1","delta":" and a little more text."}"#,
        2,
    ),
    (r#"{"type":"TEXT_MESSAGE_END","messageId":"msg-1"}"#, 1),
    (
        r#"{"type":"TOOL_CALL_START","toolCallId":"call-1","toolCallName":"search","parentMessageId":"msg-1"}"#,
        3,
    ),
    (
        r#"{"type":"TOOL_CALL_ARGS","toolCallId":"call-1","delta":"{}"}"#,
        2,
    ),
    (r#"{"type":"TOOL_CALL_END","toolCallId":"call-1"}"#, 1),
    (
        r#"{"type":"REASONING_MESSAGE_CONTENT","messageId":"r-1","delta":"weighing the options"}"#,
        2,
    ),
    (
        r#"{"type":"RUN_FINISHED","threadId":"thread-1","runId":"run-1"}"#,
        2,
    ),
];

struct Sample {
    ns_per_event: Vec<f64>,
    allocs_per_event: f64,
}

fn measure(iterations: usize, events: usize, mut f: impl FnMut()) -> Sample {
    for _ in 0..iterations.min(10) {
        f();
    }
    let mut ns_per_event = Vec::with_capacity(iterations);
    let mut allocations = 0usize;
    for _ in 0..iterations {
        let region = Region::new(GLOBAL);
        let start = Instant::now();
        f();
        let elapsed = start.elapsed();
        let change = region.change();
        allocations += change.allocations + change.reallocations;
        ns_per_event.push(elapsed.as_nanos() as f64 / events as f64);
    }
    ns_per_event.sort_by(f64::total_cmp);
    Sample {
        ns_per_event,
        allocs_per_event: allocations as f64 / (iterations * events) as f64,
    }
}

fn report(name: &str, scale: usize, bytes_per_event: f64, sample: &Sample) {
    let median = sample.ns_per_event[sample.ns_per_event.len() / 2];
    let p95 = sample.ns_per_event[sample.ns_per_event.len() * 95 / 100];
    let mbps = bytes_per_event / median * 1e3;
    println!(
        "{name:<24} x{scale}: median {median:>7.1} ns/event  p95 {p95:>7.1}  {mbps:>7.1} MB/s  {:.3} allocs/event",
        sample.allocs_per_event
    );
}

fn main() {
    let quick = std::env::args().any(|a| a == "--quick");
    let iterations = if quick { 50 } else { 500 };

    let budget: usize = TYPICAL.iter().map(|(_, n)| n).sum();
    let budget_per_event = budget as f64 / TYPICAL.len() as f64;
    let bytes_per_event =
        TYPICAL.iter().map(|(j, _)| j.len()).sum::<usize>() as f64 / TYPICAL.len() as f64;

    let mut failures = Vec::new();
    let mut parse_allocs = Vec::new();
    let mut write_allocs = Vec::new();
    for scale in [1usize, 8] {
        let stream: Vec<&str> = (0..scale * 100)
            .flat_map(|_| TYPICAL.iter().map(|(j, _)| *j))
            .collect();

        let parse = measure(iterations, stream.len(), || {
            for json in &stream {
                let event: Event = serde_json::from_str(json).unwrap_or_else(|e| panic!("{e}"));
                black_box(event);
            }
        });
        report("parse", scale, bytes_per_event, &parse);
        if parse.allocs_per_event > budget_per_event + 1e-9 {
            failures.push(format!(
                "parse x{scale}: {:.3} allocs/event over budget {budget_per_event:.3}",
                parse.allocs_per_event
            ));
        }
        parse_allocs.push(parse.allocs_per_event);

        let events: Vec<Event> = stream
            .iter()
            .map(|j| serde_json::from_str(j).unwrap_or_else(|e| panic!("{e}")))
            .collect();
        let mut buffer = Vec::with_capacity(256);
        let write = measure(iterations, events.len(), || {
            for event in &events {
                event.write_json(&mut buffer);
                black_box(&buffer);
            }
        });
        report("write_json (reused buf)", scale, bytes_per_event, &write);
        if write.allocs_per_event != 0.0 {
            failures.push(format!(
                "write_json x{scale}: {:.3} allocs/event, budget 0",
                write.allocs_per_event
            ));
        }
        write_allocs.push(write.allocs_per_event);
    }
    if parse_allocs[0] != parse_allocs[1] || write_allocs[0] != write_allocs[1] {
        failures.push("allocations per event changed with stream size".to_owned());
    }
    if !failures.is_empty() {
        eprintln!("{}", failures.join("\n"));
        std::process::exit(1);
    }
}
