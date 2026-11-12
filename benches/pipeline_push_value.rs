//! `Processor::push_value`, the middleware path, over the same stream as
//! `pipeline_push_json`. Budget: the JSON path's count minus the strings the
//! JSON parse allocates (the values are built outside the measured region
//! and moved in); see `benches/budgets.toml`.

mod support;

use std::sync::Arc;

use agui_sdk::diagnostics::{NullSink, Reporter};
use agui_sdk::pipeline::{PipelineConfig, Processor};
use support::{typical_stream, Bench, CountingAllocator, Work};

#[global_allocator]
static ALLOC: CountingAllocator = CountingAllocator;

fn main() {
    let mut bench = Bench::new("pipeline_push_value");
    bench.case(
        "text_heavy",
        300,
        |runs| {
            let events = typical_stream(runs);
            let bytes: u64 = events.iter().map(|e| e.to_string().len() as u64).sum();
            let prepare = move || events.clone();
            let run = move |events: Vec<serde_json::Value>| {
                let mut processor = Processor::new(PipelineConfig::default());
                let mut reporter = Reporter::new(Arc::new(NullSink), 256);
                let mut out = Vec::with_capacity(4);
                let mut work = Work { events: 0, bytes };
                for e in events {
                    out.clear();
                    processor
                        .push_value(e, &mut reporter, &mut out)
                        .unwrap_or_else(|e| panic!("{e}"));
                    work.events += 1;
                }
                work
            };
            (prepare, run)
        },
        None,
    );
    bench.finish();
}
