//! `SseDecoder` over a stream of typical events, chunked 1 KiB and 64 KiB.
//! Budget: 0 allocations per event in steady state (TEST-PLAN §9.1).

mod support;

use agui_sdk::client::transport::sse::SseDecoder;
use agui_sdk::wire::Chunk;
use agui_test_support::mock::sse_body;
use support::{typical_stream, Bench, CountingAllocator, Work};

#[global_allocator]
static ALLOC: CountingAllocator = CountingAllocator;

fn main() {
    let mut bench = Bench::new("sse_decode");
    for (case, chunk) in [("1KiB", 1024usize), ("64KiB", 64 * 1024)] {
        // ~1 MB of SSE text at ×1.
        bench.case(
            case,
            200,
            |runs| {
                let stream = typical_stream(runs);
                let expected = stream.len() as u64;
                let body = Chunk::from(sse_body(&stream));
                let chunks: Vec<Chunk> = (0..body.len())
                    .step_by(chunk)
                    .map(|i| body.slice(i..(i + chunk).min(body.len())))
                    .collect();
                let prepare = move || chunks.clone();
                let run = move |chunks: Vec<Chunk>| {
                    let mut decoder = SseDecoder::new(16 * 1024 * 1024);
                    let mut work = Work::default();
                    for c in chunks {
                        work.bytes += c.len() as u64;
                        decoder.push(c);
                        loop {
                            match decoder.next_event() {
                                Ok(Some(event)) => {
                                    std::hint::black_box(event);
                                    work.events += 1;
                                }
                                Ok(None) => break,
                                Err(e) => panic!("sse decode failed: {:?}: {e}", e.code()),
                            }
                        }
                    }
                    assert!(!decoder.has_partial(), "the stream ended mid-event");
                    assert_eq!(work.events, expected, "decoded event count");
                    work
                };
                (prepare, run)
            },
            None,
        );
    }
    bench.finish();
}
