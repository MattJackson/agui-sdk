//! Chunk expansion and consumer verification: per-event allocations and
//! linearity (budgets, TEST-PLAN §9).
//!
//! `cargo bench --bench pipeline_linearity [-- --quick]`.
//!
//! Like every bench here (benches/support/mod.rs, "Modes"), it gates only
//! under `cargo bench`, which passes `--bench`. Under `cargo test --benches`
//! or `--all-targets` it is a smoke run: each measurement once on a small
//! input, nothing gated. A timing ratio from an unoptimised test build on a
//! shared runner measures the runner, not the pipeline (the qa `test` job saw
//! push_json at x1.29 in debug while the release gate measured x1.01).
//!
//! Two measurements, each at stream sizes x1, x2, x4 and x8 (runs of a
//! text-heavy turn, chunked and explicit, parent and subagent lanes):
//!
//! * **stages**: [`ChunkExpander`] and [`ConsumerVerifier`] over pre-built
//!   events. Budget: 0 allocations per event in steady state (ids are
//!   reference-counted, tables are reused across runs).
//! * **push_json**: the whole [`Processor`] from JSON bytes. Reported, and
//!   held to "the same allocations per event at every size": what the
//!   enforcement parse keeps plus nothing that grows with the stream.
//!
//! Fails when a budget is exceeded or when the best time per event at x8
//! exceeds x1 by more than `[gate] time_linearity` of `budgets.toml`
//! (super-linear growth).

// Bench code: a failed unwrap is a failed bench.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::alloc::System;
use std::hint::black_box;
use std::sync::Arc;
use std::time::Instant;

use agui_sdk::diagnostics::{NullSink, Reporter};
use agui_sdk::event::Event;
use agui_sdk::pipeline::chunks::ChunkExpander;
use agui_sdk::pipeline::verify::ConsumerVerifier;
use agui_sdk::pipeline::{PipelineConfig, Processor};
use stats_alloc::{Region, StatsAlloc, INSTRUMENTED_SYSTEM};

mod support;

#[global_allocator]
static GLOBAL: &StatsAlloc<System> = &INSTRUMENTED_SYSTEM;

/// One run on the wire, `{n}` replaced by the run number so ids are fresh
/// per run, as on a replayed thread.
const RUN: &[&str] = &[
    r#"{"type":"RUN_STARTED","threadId":"thread-1","runId":"run-{n}"}"#,
    r#"{"type":"SUBAGENT_STARTED","subagentRunId":"sub-{n}","name":"researcher"}"#,
    r#"{"type":"TEXT_MESSAGE_START","messageId":"msg-{n}","role":"assistant"}"#,
    r#"{"type":"TEXT_MESSAGE_CONTENT","messageId":"msg-{n}","delta":"Hello, here is the answer you asked for"}"#,
    r#"{"type":"TEXT_MESSAGE_CHUNK","messageId":"sub-msg-{n}","delta":"the subagent ","subagentRunId":"sub-{n}"}"#,
    r#"{"type":"TEXT_MESSAGE_CONTENT","messageId":"msg-{n}","delta":" and a little more text."}"#,
    r#"{"type":"TEXT_MESSAGE_CHUNK","delta":"streams too","subagentRunId":"sub-{n}"}"#,
    r#"{"type":"TEXT_MESSAGE_END","messageId":"msg-{n}"}"#,
    r#"{"type":"SUBAGENT_FINISHED","subagentRunId":"sub-{n}"}"#,
    r#"{"type":"TOOL_CALL_CHUNK","toolCallId":"call-{n}","toolCallName":"search","parentMessageId":"msg-{n}","delta":"{\"q\":"}"#,
    r#"{"type":"TOOL_CALL_CHUNK","delta":"\"agui\"}"}"#,
    r#"{"type":"STEP_STARTED","stepName":"think"}"#,
    r#"{"type":"REASONING_MESSAGE_CHUNK","messageId":"r-{n}","delta":"weighing the options"}"#,
    r#"{"type":"REASONING_MESSAGE_CHUNK","delta":" carefully"}"#,
    r#"{"type":"STEP_FINISHED","stepName":"think"}"#,
    r#"{"type":"RUN_FINISHED","threadId":"thread-1","runId":"run-{n}"}"#,
];

/// Runs per x1 stream.
const BASE_RUNS: usize = 200;

fn frames(runs: usize) -> Vec<Vec<u8>> {
    (0..runs)
        .flat_map(|n| {
            RUN.iter()
                .map(move |f| f.replace("{n}", &n.to_string()).into_bytes())
        })
        .collect()
}

fn events(frames: &[Vec<u8>]) -> Vec<Event> {
    frames
        .iter()
        .map(|f| serde_json::from_slice(f).unwrap())
        .collect()
}

struct Sample {
    ns_per_event: f64,
    allocs_per_event: f64,
}

/// Expansion and verification only. Events are built one run at a time
/// outside the measured span (so neither the cost of building them nor the
/// cache footprint of a whole pre-built stream is measured); what is timed
/// and counted is the stages' own work.
fn stages(runs: usize, iterations: usize) -> Sample {
    let frames = frames(runs);
    let mut best = f64::MAX;
    let mut allocs = 0usize;
    let mut counted = 0usize;
    for i in 0..iterations + 1 {
        let mut expander = ChunkExpander::new();
        let mut verifier = ConsumerVerifier::new();
        let mut out = Vec::with_capacity(8);
        let mut elapsed = std::time::Duration::ZERO;
        let region = Region::new(GLOBAL);
        for (n, run) in frames.chunks(RUN.len()).enumerate() {
            let input = events(run);
            let before = region.change();
            let start = Instant::now();
            for e in input {
                out.clear();
                expander.push(e, &mut out).unwrap();
                for e in &out {
                    verifier.observe(e).unwrap();
                }
            }
            elapsed += start.elapsed();
            let after = region.change();
            out.clear();
            // The first run warms the tables, as on any long stream.
            if i > 0 && n > 0 {
                allocs += (after.allocations + after.reallocations)
                    - (before.allocations + before.reallocations);
                counted += run.len();
            }
        }
        black_box(&verifier);
        if i > 0 {
            best = best.min(elapsed.as_nanos() as f64 / frames.len() as f64);
        }
    }
    Sample {
        ns_per_event: best,
        allocs_per_event: allocs as f64 / counted as f64,
    }
}

/// The whole processor from bytes.
fn push_json(runs: usize, iterations: usize) -> Sample {
    let frames = frames(runs);
    let mut best = f64::MAX;
    let mut allocs = 0usize;
    let mut counted = 0usize;
    for i in 0..iterations + 1 {
        let mut processor = Processor::new(PipelineConfig::default());
        let mut reporter = Reporter::new(Arc::new(NullSink), 256);
        let mut out = Vec::with_capacity(8);
        let region = Region::new(GLOBAL);
        let start = Instant::now();
        for frame in &frames {
            out.clear();
            processor.push_json(frame, &mut reporter, &mut out).unwrap();
        }
        let elapsed = start.elapsed();
        let stats = region.change();
        black_box(processor.reducer().messages().len());
        if i > 0 {
            best = best.min(elapsed.as_nanos() as f64 / frames.len() as f64);
            allocs += stats.allocations + stats.reallocations;
            counted += frames.len();
        }
    }
    Sample {
        ns_per_event: best,
        allocs_per_event: allocs as f64 / counted as f64,
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if !args.iter().any(|a| a == "--bench") {
        // Smoke: one small pass per measurement, nothing gated. Four runs
        // keep `stages` counting (it skips run 0 and pass 0).
        for (name, measure) in [
            ("stages", stages as fn(usize, usize) -> Sample),
            ("push_json", push_json as fn(usize, usize) -> Sample),
        ] {
            let s = black_box(measure(4, 1));
            println!(
                "pipeline_linearity/{name}: smoke ok ({:.3} allocs/event)",
                s.allocs_per_event
            );
        }
        return;
    }
    let quick = args.iter().any(|a| a == "--quick");
    let limit = support::gate_time_linearity();
    let iterations = if quick { 3 } else { 15 };
    let sizes: &[usize] = if quick { &[1, 8] } else { &[1, 2, 4, 8] };
    let mut failures = Vec::new();

    for (name, measure) in [
        ("stages", stages as fn(usize, usize) -> Sample),
        ("push_json", push_json as fn(usize, usize) -> Sample),
    ] {
        let samples: Vec<(usize, Sample)> = sizes
            .iter()
            .map(|&x| (x, measure(BASE_RUNS * x, iterations)))
            .collect();
        for (x, s) in &samples {
            println!(
                "{name:>9} x{x}: {:>8.1} ns/event  {:>6.3} allocs/event",
                s.ns_per_event, s.allocs_per_event
            );
        }
        let first = &samples[0].1;
        let last = &samples[samples.len() - 1].1;
        let ratio = last.ns_per_event / first.ns_per_event;
        println!("{name:>9} time per event x8/x1 = {ratio:.3} (limit {limit})");
        if ratio > limit {
            failures.push(format!("{name}: super-linear ({ratio:.3})"));
        }
        if name == "stages" && samples.iter().any(|(_, s)| s.allocs_per_event > 0.01) {
            failures.push(format!("{name}: allocations per event above 0"));
        }
        // Per-event allocations must not grow with the stream.
        if last.allocs_per_event > first.allocs_per_event * 1.02 + 0.01 {
            failures.push(format!(
                "{name}: allocations per event grow ({:.3} -> {:.3})",
                first.allocs_per_event, last.allocs_per_event
            ));
        }
    }
    if !failures.is_empty() {
        eprintln!("budget failures: {failures:?}");
        std::process::exit(1);
    }
}
