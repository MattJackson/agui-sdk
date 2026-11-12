//! `proto_codec` (TEST-PLAN §9.1): protobuf framing, decoding and encoding,
//! with time per event and heap allocations per event.
//!
//! Run: `cargo bench --bench proto_codec --features proto` (add
//! `preserve-order` to compare the `Value` paths). `BENCH_MS` sets the time
//! per row (default 300).
//!
//! Modes, as for the other benches (benches/support/mod.rs): with `--bench`
//! (`cargo bench`) every row is measured and the budgets below are gated
//! (`encode_value` is printed, not gated); without it (`cargo test
//! --benches` / `--all-targets`) it is a smoke run, each row a few calls
//! after as many warm-up calls, printed and not gated.
//!
//! Allocation budgets (API.md §8) are asserted under
//! `--bench`, so a regression fails the bench:
//!
//! * `FrameDecoder` over a stream chunked at 4 KiB: 0 allocations per event
//!   once warm (in-chunk frames are slices; split frames reuse one buffer);
//! * `encode_json` into a reused buffer: 0 per event once warm;
//! * `decode_event_json` into a reused `String`: 0 per event once warm;
//! * `decode_event`: only the resulting value's own storage (strings, keys,
//!   containers and their amortized growth), nothing else.
//!
//! Time figures are printed, never asserted. The counting allocator is
//! `stats_alloc`'s instrumented system allocator.

#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use std::alloc::System;
use std::hint::black_box;
use std::time::{Duration, Instant};

use agui_sdk::proto::decode::{decode_event, decode_event_json, Decoded};
use agui_sdk::proto::encode::{encode_json, encode_value};
use agui_sdk::proto::framing::{write_frame, FrameDecoder, DEFAULT_MAX_FRAME_BYTES};
use agui_sdk::wire::Chunk;
use serde_json::Value;
use stats_alloc::{Region, StatsAlloc, INSTRUMENTED_SYSTEM};

#[global_allocator]
static GLOBAL: &StatsAlloc<System> = &INSTRUMENTED_SYSTEM;

#[path = "../tests/proto_support/mod.rs"]
mod proto_support;

struct Load {
    name: &'static str,
    json: Vec<String>,
}

/// The byte corpus' 98 events, compacted with member order kept.
fn corpus() -> Option<Vec<String>> {
    let cases = proto_support::byte_cases()?;
    Some(
        cases
            .iter()
            .map(|c| {
                let mut out = String::new();
                compact(std::str::from_utf8(&c.json).unwrap(), &mut out);
                out
            })
            .collect(),
    )
}

/// Strips insignificant whitespace from JSON text, keeping member order
/// whatever `serde_json`'s map order is.
fn compact(src: &str, out: &mut String) {
    let mut in_str = false;
    let mut escaped = false;
    for c in src.chars() {
        if in_str {
            out.push(c);
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == '"' {
                in_str = false;
            }
        } else if c == '"' {
            in_str = true;
            out.push(c);
        } else if !c.is_whitespace() {
            out.push(c);
        }
    }
}

fn streaming() -> Vec<String> {
    (0..64)
        .map(|i| {
            format!(
                r#"{{"type":"TEXT_MESSAGE_CONTENT","messageId":"msg_01HZX3","delta":"token {i} of a streamed reply, "}}"#
            )
        })
        .collect()
}

fn large() -> Vec<String> {
    let mut msgs = Vec::new();
    for i in 0..200 {
        msgs.push(format!(
            r#"{{"id":"m{i}","role":"assistant","content":"Answer number {i}, with some text in it.","toolCalls":[{{"id":"c{i}","type":"function","function":{{"name":"search","arguments":"{{\"q\":\"item {i}\"}}"}}}}]}}"#
        ));
    }
    let snapshot = format!(
        r#"{{"type":"MESSAGES_SNAPSHOT","messages":[{}]}}"#,
        msgs.join(",")
    );
    let mut state = String::from(r#"{"type":"STATE_SNAPSHOT","snapshot":{"#);
    for i in 0..500 {
        if i > 0 {
            state.push(',');
        }
        state.push_str(&format!(
            r#""key{i}":{{"value":{i},"label":"label {i}","flags":[true,false,null],"ratio":0.{i}}}"#
        ));
    }
    state.push_str("}}");
    vec![snapshot, state]
}

/// The allocation budget for building `v`: one per owned string (values and
/// keys), plus each container's own storage and its amortized growth,
/// `2 + 2·⌈log2(len + 1)⌉` (a map may hold two buffers, each grown by
/// doubling).
fn owned_allocations(v: &Value) -> u64 {
    fn growth(len: usize) -> u64 {
        2 + 2 * u64::from(usize::BITS - len.leading_zeros())
    }
    match v {
        Value::Array(a) => growth(a.len()) + a.iter().map(owned_allocations).sum::<u64>(),
        Value::Object(o) => {
            growth(o.len()) + o.iter().map(|(_, v)| 1 + owned_allocations(v)).sum::<u64>()
        }
        Value::String(_) => 1,
        _ => 0,
    }
}

/// Warms `f` up, then runs it until `budget` is spent. Returns the best
/// batch's seconds per call and the allocations (plus reallocations) per
/// call over the measured run. A zero budget (smoke) is three warm-up calls
/// and three measured ones.
fn measure(budget: Duration, mut f: impl FnMut()) -> (f64, f64) {
    let start = Instant::now();
    let min_warm_time = if budget.is_zero() {
        Duration::ZERO
    } else {
        Duration::from_millis(10)
    };
    let mut warm = 0u64;
    while warm < 3 || start.elapsed() < min_warm_time {
        f();
        warm += 1;
    }
    let region = Region::new(GLOBAL);
    let t0 = Instant::now();
    let mut best = f64::INFINITY;
    let mut calls = 0u64;
    while calls == 0 || t0.elapsed() < budget {
        let t = Instant::now();
        for _ in 0..warm {
            f();
        }
        best = best.min(t.elapsed().as_secs_f64() / warm as f64);
        calls += warm;
    }
    let change = region.change();
    let allocs = (change.allocations + change.reallocations) as f64;
    (best, allocs / calls as f64)
}

/// Prints one row and returns the allocations per event.
fn row(load: &Load, what: &str, bytes: usize, budget: Duration, f: impl FnMut()) -> f64 {
    let events = load.json.len() as f64;
    let (secs, allocs) = measure(budget, f);
    let per_event = allocs / events;
    println!(
        "{:<10} {:<34} {:>8.0} MB/s {:>9.0} ns/event {:>8.2} allocs/event",
        load.name,
        what,
        bytes as f64 / secs / 1e6,
        secs * 1e9 / events,
        per_event,
    );
    per_event
}

fn main() {
    let gated = std::env::args().any(|a| a == "--bench");
    let budget = if gated {
        Duration::from_millis(
            std::env::var("BENCH_MS")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(300),
        )
    } else {
        Duration::ZERO
    };
    let mut loads = vec![
        Load {
            name: "streaming",
            json: streaming(),
        },
        Load {
            name: "large",
            json: large(),
        },
    ];
    if let Some(json) = corpus() {
        loads.insert(
            0,
            Load {
                name: "corpus",
                json,
            },
        );
    }
    println!(
        "preserve-order: {}. MB/s is JSON text in (encode) or protobuf bytes in (decode).",
        cfg!(feature = "preserve-order")
    );

    for load in &loads {
        let json_bytes: usize = load.json.iter().map(String::len).sum();
        let bins: Vec<Vec<u8>> = load
            .json
            .iter()
            .map(|s| {
                let mut out = Vec::new();
                encode_json(s.as_bytes(), &mut out).unwrap();
                out
            })
            .collect();
        let pb_bytes: usize = bins.iter().map(Vec::len).sum();
        let values: Vec<Value> = load
            .json
            .iter()
            .map(|s| serde_json::from_str(s).unwrap())
            .collect();
        let mut framed = Vec::new();
        for b in &bins {
            write_frame(b, &mut framed).unwrap();
        }
        // Shared chunks: cloning one per iteration is a refcount bump, as a
        // transport handing over its received buffers would be.
        let whole = Chunk::from(framed);
        let chunks: Vec<Chunk> = (0..whole.len())
            .step_by(4096)
            .map(|at| whole.slice(at..(at + 4096).min(whole.len())))
            .collect();

        let mut out = Vec::new();
        let encode_allocs = row(load, "encode_json (reused out)", json_bytes, budget, || {
            for s in &load.json {
                out.clear();
                encode_json(black_box(s.as_bytes()), &mut out).unwrap();
                black_box(&out);
            }
        });
        row(
            load,
            "encode_value (reused out)",
            json_bytes,
            budget,
            || {
                for v in &values {
                    out.clear();
                    encode_value(black_box(v), &mut out).unwrap();
                    black_box(&out);
                }
            },
        );

        let mut decoder = FrameDecoder::new(DEFAULT_MAX_FRAME_BYTES);
        let framing_allocs = row(load, "FrameDecoder, 4 KiB chunks", pb_bytes, budget, || {
            for c in &chunks {
                decoder.push(c.clone());
                while let Some(frame) = decoder.next_frame().unwrap() {
                    black_box(frame);
                }
            }
            assert!(!decoder.has_partial());
        });

        let mut text = String::new();
        let json_allocs = row(load, "decode_event_json (reused)", pb_bytes, budget, || {
            for b in &bins {
                text.clear();
                decode_event_json(black_box(b), &mut text).unwrap();
                black_box(&text);
            }
        });

        let owned: u64 = values.iter().map(owned_allocations).sum();
        let value_allocs = row(load, "decode_event -> Value", pb_bytes, budget, || {
            for b in &bins {
                match decode_event(black_box(b)).unwrap() {
                    Decoded::Event(v) => drop(black_box(v)),
                    other => panic!("not an event: {other:?}"),
                }
            }
        });
        println!();

        if !gated {
            continue;
        }
        assert_eq!(encode_allocs, 0.0, "{}: encode_json allocates", load.name);
        assert_eq!(framing_allocs, 0.0, "{}: framing allocates", load.name);
        assert_eq!(
            json_allocs, 0.0,
            "{}: decode_event_json allocates",
            load.name
        );
        let budget_per_event = owned as f64 / load.json.len() as f64;
        assert!(
            value_allocs <= budget_per_event,
            "{}: decode_event made {value_allocs:.2} allocations per event, over the budget of {budget_per_event:.2}",
            load.name
        );
    }
    if !gated {
        println!("proto_codec: smoke ok (not gated; run `cargo bench` to gate)");
    }
}
