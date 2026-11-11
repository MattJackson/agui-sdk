//! Producer path: `ProducerVerifier::check` plus the wire encoder into a
//! reused buffer, for SSE and protobuf. Budget: 0 allocations per event in
//! the encoder (TEST-PLAN §9.1).

mod support;

use agui_sdk::pipeline::verify::ProducerVerifier;
use agui_sdk::server::encode::Encoder;
use agui_sdk::wire::WireFormat;
use agui_sdk::Event;
use support::{typical_stream, Bench, CountingAllocator, Work};

#[global_allocator]
static ALLOC: CountingAllocator = CountingAllocator;

fn main() {
    let mut bench = Bench::new("server_emit_encode");
    for (case, format) in [("sse", WireFormat::Sse), ("proto", WireFormat::Protobuf)] {
        bench.case(
            case,
            300,
            |runs| {
                let events: Vec<Event> = typical_stream(runs)
                    .into_iter()
                    .map(|v| serde_json::from_value(v).unwrap_or_else(|e| panic!("{e}")))
                    .collect();
                let prepare = || ();
                let run = move |()| {
                    let mut verifier = ProducerVerifier::new();
                    let mut encoder = Encoder::for_format(format);
                    let mut buf = Vec::with_capacity(64 * 1024);
                    let mut work = Work::default();
                    for e in &events {
                        verifier.check(e).unwrap_or_else(|v| panic!("{v:?}"));
                        buf.clear();
                        encoder
                            .encode(e, &mut buf)
                            .unwrap_or_else(|e| panic!("{e}"));
                        work.bytes += buf.len() as u64;
                        work.events += 1;
                    }
                    work
                };
                (prepare, run)
            },
            None,
        );
    }
    bench.finish();
}
