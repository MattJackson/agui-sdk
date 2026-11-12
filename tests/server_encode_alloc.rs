//! Allocation budget of the producer's wire encoders : once
//! the output buffer has grown, encoding an event allocates nothing, for
//! every format; and a response body coalesces every ready event into one
//! chunk.
//!
//! Allocations are counted on the measuring thread only
//! (`agui_test_support::alloc`), so the harness's own threads cannot land in
//! a window. The tests still serialise with a lock.

#![cfg(feature = "server")]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Mutex;

use agui_sdk::server::encode::json::NdjsonEncoder;
use agui_sdk::server::encode::Encoder;
use agui_sdk::wire::WireFormat;
use agui_sdk::Event;
use agui_test_support::alloc::{self, CountingAllocator};
use serde_json::json;

#[global_allocator]
static GLOBAL: CountingAllocator = CountingAllocator;

static SERIAL: Mutex<()> = Mutex::new(());

fn typical() -> Vec<Event> {
    [
        json!({"type": "RUN_STARTED", "threadId": "t1", "runId": "r1", "protocolVersion": "1.0"}),
        json!({"type": "TEXT_MESSAGE_START", "messageId": "m1", "role": "assistant"}),
        json!({"type": "TEXT_MESSAGE_CONTENT", "messageId": "m1", "delta": "Hello, \"world\"\n é"}),
        json!({"type": "TEXT_MESSAGE_END", "messageId": "m1"}),
        json!({"type": "TOOL_CALL_START", "toolCallId": "c1", "toolCallName": "lookup", "parentMessageId": "m1"}),
        json!({"type": "TOOL_CALL_ARGS", "toolCallId": "c1", "delta": "{\"q\":1}"}),
        json!({"type": "TOOL_CALL_END", "toolCallId": "c1"}),
        json!({"type": "STATE_DELTA", "delta": [{"op": "replace", "path": "/n", "value": {"a": [1, 2.5, null]}}]}),
        json!({"type": "CUSTOM", "name": "vendor:x", "value": {"k": "v"}, "metadata": {"m": 1}}),
        json!({"type": "RUN_FINISHED", "threadId": "t1", "runId": "r1", "outcome": {"type": "success"}}),
    ]
    .into_iter()
    .map(|v| serde_json::from_value(v).unwrap())
    .collect()
}

fn steady_state_allocations(mut encode: impl FnMut(&Event, &mut Vec<u8>)) -> u64 {
    let events = typical();
    let mut out = Vec::new();
    // Warm-up: grow the buffer and any per-thread scratch.
    for _ in 0..3 {
        for event in &events {
            out.clear();
            encode(event, &mut out);
        }
    }
    assert!(
        alloc::installed(),
        "the counting allocator is not installed"
    );
    // The quietest of several windows: a real per-event allocation shows up
    // 100 times over in every one of them, so the minimum cannot hide it.
    (0..5)
        .map(|_| {
            let ((), stats) = alloc::measure(|| {
                for _ in 0..100 {
                    for event in &events {
                        out.clear();
                        encode(event, &mut out);
                    }
                }
            });
            stats.total()
        })
        .min()
        .unwrap_or(u64::MAX)
}

#[test]
fn sse_encoding_allocates_nothing_in_steady_state() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let mut encoder = Encoder::for_format(WireFormat::Sse);
    let n = steady_state_allocations(|e, out| encoder.encode(e, out).unwrap());
    assert_eq!(n, 0);
}

#[cfg(feature = "proto")]
#[test]
fn protobuf_encoding_allocates_nothing_in_steady_state() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let mut encoder = Encoder::for_format(WireFormat::Protobuf);
    let n = steady_state_allocations(|e, out| encoder.encode(e, out).unwrap());
    assert_eq!(n, 0);
}

/// The fewest allocations `f` makes in one of several windows of 100 calls,
/// once warmed up.
#[cfg(feature = "proto")]
fn quietest_window(mut f: impl FnMut()) -> u64 {
    for _ in 0..3 {
        f();
    }
    assert!(
        alloc::installed(),
        "the counting allocator is not installed"
    );
    (0..5)
        .map(|_| {
            alloc::measure(|| {
                for _ in 0..100 {
                    f();
                }
            })
            .1
            .total()
        })
        .min()
        .unwrap_or(u64::MAX)
}

/// The protobuf codec's open-JSON paths a warm codec takes without
/// allocating, as its docs say: array-index keys (JS order puts them
/// first), escaped keys and enum spellings, and objects large enough to be
/// deduplicated by sorting, in both directions.
#[cfg(feature = "proto")]
#[test]
fn protobuf_open_json_allocates_nothing_in_steady_state() {
    use agui_sdk::proto::decode::decode_event_json;
    use agui_sdk::proto::encode::{encode_json, encode_value};
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let wide: String = (0..40).map(|i| format!(r#","k\u0030{i}":{i}"#)).collect();
    let texts = [
        r#"{"type":"STATE_SNAPSHOT","snapshot":{"b":1,"0":2,"a":{"1":null}}}"#.to_owned(),
        r#"{"type":"STATE_SNAPSHOT","snapshot":{"\u0061":1,"\u0030":2,"a":3}}"#.to_owned(),
        format!(r#"{{"type":"STATE_SNAPSHOT","snapshot":{{"7":0{wide},"k\u00300":1}}}}"#),
        r#"{"type":"STATE_DELTA","delta":[{"op":"\u0061dd","path":"/a","value":1}]}"#.to_owned(),
    ];
    let mut failures = Vec::new();
    let mut out = Vec::new();
    for text in &texts {
        let n = quietest_window(|| {
            out.clear();
            encode_json(text.as_bytes(), &mut out).unwrap();
        });
        if n != 0 {
            failures.push(format!("encode_json {text}: {n}"));
        }
        let value: serde_json::Value = serde_json::from_str(text).unwrap();
        let n = quietest_window(|| {
            out.clear();
            encode_value(&value, &mut out).unwrap();
        });
        if n != 0 {
            failures.push(format!("encode_value {text}: {n}"));
        }
        out.clear();
        encode_json(text.as_bytes(), &mut out).unwrap();
        let frame = out.clone();
        let mut json = String::new();
        let n = quietest_window(|| {
            json.clear();
            decode_event_json(&frame, &mut json).unwrap();
        });
        if n != 0 {
            failures.push(format!("decode_event_json {text}: {n}"));
        }
    }
    assert!(failures.is_empty(), "allocations per 100: {failures:#?}");
}

#[test]
fn ndjson_encoding_allocates_nothing_in_steady_state() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let mut encoder = NdjsonEncoder::new();
    let n = steady_state_allocations(|e, out| encoder.encode(e, out).unwrap());
    assert_eq!(n, 0);
}

#[test]
fn a_response_body_coalesces_ready_events_into_few_chunks() {
    use agui_sdk::server::http::{handle, HttpRequest, ServeConfig};
    use agui_sdk::server::{BoxError, CancelToken, RunContext, RunOutcome, Runner};

    struct Burst;
    impl agui_sdk::server::Agent for Burst {
        type State = serde_json::Value;
        async fn run(&self, ctx: RunContext) -> Result<RunOutcome, BoxError> {
            for i in 0..200 {
                ctx.emitter().custom("vendor:tick", json!(i)).await?;
            }
            Ok(RunOutcome::success())
        }
    }

    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let runner = Runner::new(Burst);
    let body = br#"{"threadId":"t1","runId":"r1","messages":[]}"#;
    let response = handle(
        &runner,
        &ServeConfig::default(),
        HttpRequest::new(Some("application/json"), None, body),
        CancelToken::new(),
    )
    .unwrap();
    let mut body = response.body;
    let rt = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();
    let (chunks, bytes) = rt.block_on(async move {
        let (mut chunks, mut bytes) = (0usize, Vec::new());
        while let Some(chunk) = body.next().await {
            chunks += 1;
            bytes.extend_from_slice(&chunk);
        }
        (chunks, bytes)
    });
    let events = bytes.windows(6).filter(|w| *w == b"data: ").count();
    assert_eq!(events, 202);
    // The agent never waits on the network here, so far fewer chunks than
    // events reach the writer (one per poll, bounded by the chunk cap).
    assert!(chunks * 10 < events, "{chunks} chunks for {events} events");
}
