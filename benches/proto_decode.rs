//! Protobuf framing and decoding of a stream of typical events.
//! Budgets: 0 allocations per event for framing; decode allocates only the
//! resulting value's nodes (TEST-PLAN §9.1).

mod support;

use agui_sdk::proto::decode::decode_event;
use agui_sdk::proto::encode::encode_json;
use agui_sdk::proto::framing::{write_frame, FrameDecoder, DEFAULT_MAX_FRAME_BYTES};
use agui_sdk::wire::Chunk;
use support::{frames, typical_stream, Bench, CountingAllocator, Work};

#[global_allocator]
static ALLOC: CountingAllocator = CountingAllocator;

fn wire(runs: usize) -> (Vec<Chunk>, Vec<Vec<u8>>) {
    let mut body = Vec::new();
    let mut payloads = Vec::new();
    for json in frames(&typical_stream(runs)) {
        let mut payload = Vec::new();
        encode_json(&json, &mut payload).unwrap_or_else(|e| panic!("encode: {e}"));
        write_frame(&payload, &mut body).unwrap_or_else(|e| panic!("frame: {e}"));
        payloads.push(payload);
    }
    let body = Chunk::from(body);
    let chunks = (0..body.len())
        .step_by(64 * 1024)
        .map(|i| body.slice(i..(i + 64 * 1024).min(body.len())))
        .collect();
    (chunks, payloads)
}

fn main() {
    let mut bench = Bench::new("proto_decode");
    bench.case(
        "framing",
        200,
        |runs| {
            let (chunks, payloads) = wire(runs);
            let expected = payloads.len() as u64;
            let prepare = move || chunks.clone();
            let run = move |chunks: Vec<Chunk>| {
                let mut decoder = FrameDecoder::new(DEFAULT_MAX_FRAME_BYTES);
                let mut work = Work::default();
                for c in chunks {
                    work.bytes += c.len() as u64;
                    decoder.push(c);
                    loop {
                        match decoder.next_frame() {
                            Ok(Some(frame)) => {
                                std::hint::black_box(frame);
                                work.events += 1;
                            }
                            Ok(None) => break,
                            Err(e) => panic!("frame decode failed: {:?}: {e}", e.code()),
                        }
                    }
                }
                assert!(!decoder.has_partial(), "the stream ended mid-frame");
                assert_eq!(work.events, expected, "decoded frame count");
                work
            };
            (prepare, run)
        },
        None,
    );
    bench.case(
        "decode",
        200,
        |runs| {
            let (_, payloads) = wire(runs);
            let prepare = || ();
            let run = move |()| {
                let mut work = Work::default();
                for p in &payloads {
                    work.bytes += p.len() as u64;
                    std::hint::black_box(decode_event(p).unwrap_or_else(|e| panic!("decode: {e}")));
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
