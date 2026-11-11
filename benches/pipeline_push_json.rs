//! `Processor::push_json`: enforcement, expansion, verification and the
//! reducer over a text-heavy stream. Ceiling: allocations per event at most
//! the strings the events own (TEST-PLAN §9.1); exact budget in budgets.toml.

mod support;

use std::sync::Arc;

use agui_sdk::diagnostics::{NullSink, Reporter};
use agui_sdk::pipeline::{PipelineConfig, Processor};
use agui_test_support::alloc::AllocStats;
use support::{frames, owned_strings, typical_stream, Bench, CountingAllocator, Work};

#[global_allocator]
static ALLOC: CountingAllocator = CountingAllocator;

fn main() {
    let mut bench = Bench::new("pipeline_push_json");
    let per_event = {
        let s = typical_stream(1);
        s.iter().map(owned_strings).sum::<u64>() as f64 / s.len() as f64
    };
    let ceiling = move |w: &Work, a: &AllocStats| {
        let limit = (per_event * w.events as f64).ceil() as u64 + 64;
        if a.total() <= limit {
            Ok(())
        } else {
            Err(format!(
                "{} allocations > {limit} (owned strings + fixed)",
                a.total()
            ))
        }
    };
    bench.case(
        "text_heavy",
        300,
        |runs| {
            let frames = frames(&typical_stream(runs));
            let prepare = || ();
            let run = move |()| {
                let mut processor = Processor::new(PipelineConfig::default());
                let mut reporter = Reporter::new(Arc::new(NullSink), 256);
                let mut out = Vec::with_capacity(4);
                let mut work = Work::default();
                for f in &frames {
                    out.clear();
                    processor
                        .push_json(f, &mut reporter, &mut out)
                        .unwrap_or_else(|e| panic!("{e}"));
                    work.events += 1;
                    work.bytes += f.len() as u64;
                }
                work
            };
            (prepare, run)
        },
        Some(&ceiling),
    );
    bench.finish();
}
