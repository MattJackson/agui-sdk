//! `HttpTransport` end to end over a local HTTP/1.1 mock server: POST, read
//! the response, decode SSE (and protobuf with `proto`) into frames.
//!
//! Reports throughput (MB/s of body, events/s) and allocations per event.
//! The count covers only the thread driving `block_on` (the allocator counts
//! per thread, so the mock server and hyper's connection tasks, which run on
//! the runtime's worker threads, are not included) and depends on how TCP
//! splits the body, so it is
//! informational, not gated: the decoders' exact budgets are gated by
//! `sse_decode` and `proto_decode`. A run fails only if a stream does not
//! arrive whole, or if time grows super-linearly from ×1 to ×8 input.
//!
//! `cargo bench --bench http_transport` measures; without `--bench` (as under
//! `cargo test --benches`) it does one small smoke pass.

mod support;

use std::time::{Duration, Instant};

use agui_sdk::client::transport::{HttpTransport, Transport, TransportRequest};
use agui_sdk::wire::Chunk;
use agui_test_support::alloc;
use agui_test_support::http_mock::{runtime, Framing, MockServer, Reply};
use agui_test_support::mock::sse_body;
use support::{typical_stream, CountingAllocator};

#[global_allocator]
static ALLOC: CountingAllocator = CountingAllocator;

/// One exchange: open, drain, count. Returns `(events, body bytes)`.
async fn exchange(transport: &HttpTransport, body_len: usize) -> (u64, u64) {
    let input = br#"{"threadId":"t-bench","runId":"r-0","state":{},"messages":[],"tools":[],"context":[],"forwardedProps":{}}"#;
    let request = TransportRequest::new(Chunk::from_static(input), "t-bench".into(), "r-0".into());
    let mut stream = transport
        .open(request)
        .await
        .unwrap_or_else(|e| panic!("open: {e}"));
    let mut events = 0;
    while let Some(frame) = stream.next().await {
        std::hint::black_box(frame.unwrap_or_else(|e| panic!("stream: {e}")));
        events += 1;
    }
    (events, body_len as u64)
}

struct Measured {
    events: u64,
    bytes: u64,
    median: Duration,
    allocs_per_event: f64,
}

fn measure(
    name: &str,
    body: Vec<u8>,
    reply: fn(Vec<u8>) -> Reply,
    expected: u64,
    passes: usize,
) -> Measured {
    let rt = runtime();
    let len = body.len();
    let server = rt
        .block_on(MockServer::always(reply(body).framing(Framing::Length)))
        .unwrap_or_else(|e| panic!("mock: {e}"));
    let transport = HttpTransport::builder(server.url("/bench"))
        .build()
        .unwrap_or_else(|e| panic!("build: {e}"));
    // Warm the pool and the allocator.
    let (events, _) = rt.block_on(exchange(&transport, len));
    assert_eq!(events, expected, "{name}: the stream did not arrive whole");
    let ((events, bytes), stats) = alloc::measure(|| rt.block_on(exchange(&transport, len)));
    let mut times: Vec<Duration> = (0..passes)
        .map(|_| {
            let start = Instant::now();
            std::hint::black_box(rt.block_on(exchange(&transport, len)));
            start.elapsed()
        })
        .collect();
    times.sort();
    Measured {
        events,
        bytes,
        median: times[times.len() / 2],
        allocs_per_event: stats.total() as f64 / events.max(1) as f64,
    }
}

fn main() {
    let measuring = std::env::args().any(|a| a == "--bench");
    let (scales, passes): (&[usize], usize) = if measuring { (&[1, 8], 9) } else { (&[1], 1) };
    let mut failures = Vec::new();
    type Case = (
        &'static str,
        fn(&[serde_json::Value]) -> Vec<u8>,
        fn(Vec<u8>) -> Reply,
    );
    let mut cases: Vec<Case> = vec![("sse", sse_body, |b| Reply::sse(b))];
    #[cfg(feature = "proto")]
    cases.push((
        "proto",
        |events| {
            agui_test_support::mock::proto_body(events)
                .unwrap_or_else(|(i, e)| panic!("event {i}: {e}"))
        },
        |b| Reply::proto(b),
    ));
    for (name, encode, reply) in cases {
        let mut points = Vec::new();
        for &scale in scales {
            // ~1 MB of SSE at ×1 (200 runs of 59 events).
            let runs = if measuring { 200 * scale } else { 4 };
            let events = typical_stream(runs);
            let m = measure(name, encode(&events), reply, events.len() as u64, passes);
            let secs = m.median.as_secs_f64();
            println!(
                "http_transport/{name} ×{scale}: {:.1} MB/s, {:.0} events/s, {:.0} ns/event, {:.3} allocs/event (driver thread only)",
                m.bytes as f64 / secs / 1e6,
                m.events as f64 / secs,
                secs * 1e9 / m.events.max(1) as f64,
                m.allocs_per_event,
            );
            points.push((scale, secs));
        }
        if let (Some(first), Some(last)) = (points.first(), points.last()) {
            let k = (last.0 / first.0) as f64;
            if k > 1.0 && last.1 / first.1 > k * 1.5 {
                failures.push(format!(
                    "http_transport/{name}: time grew ×{:.2} for ×{k} input: super-linear",
                    last.1 / first.1
                ));
            }
        }
    }
    for f in &failures {
        eprintln!("PERF GATE: {f}");
    }
    std::process::exit(i32::from(!failures.is_empty()));
}
