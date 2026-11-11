//! Shared helpers for the HTTP transport tests: inputs, event scripts, wire
//! encodings, draining a frame stream, and judging it with the pipeline.

#![allow(dead_code)]

use std::sync::Arc;

use agui_sdk::client::transport::TransportRequest;
use agui_sdk::client::FrameStream;
use agui_sdk::diagnostics::{CollectingSink, Reporter};
use agui_sdk::pipeline::{PipelineConfig, Processor, Termination};
use agui_sdk::wire::Chunk;
use agui_sdk::Error;
use serde_json::{json, Value};

#[allow(unused_imports)]
pub use agui_test_support::http_mock::{
    runtime, tls, Captured, Framing, MockProxy, MockServer, Reply, SilentServer,
};

/// Sleeps on the test runtime.
pub async fn sleep(ms: u64) {
    agui_test_support::http_mock::tokio::time::sleep(std::time::Duration::from_millis(ms)).await;
}

/// Runs `f` to completion on a fresh runtime.
pub fn block<F: std::future::Future>(f: F) -> F::Output {
    runtime().block_on(f)
}

/// The run input every test sends.
pub fn input() -> Value {
    json!({
        "threadId": "t1",
        "runId": "r1",
        "state": {},
        "messages": [{"id": "u1", "role": "user", "content": "hi"}],
        "tools": [],
        "context": [],
        "forwardedProps": {}
    })
}

/// A transport request carrying [`input`].
pub fn request() -> TransportRequest {
    let body = serde_json::to_vec(&input()).unwrap_or_default();
    TransportRequest::new(Chunk::from(body), "t1".into(), "r1".into())
}

/// One complete run: start, one text message, finish.
pub fn run(thread: &str, run: &str, text: &str) -> Vec<Value> {
    vec![
        json!({"type": "RUN_STARTED", "threadId": thread, "runId": run}),
        json!({"type": "TEXT_MESSAGE_START", "messageId": format!("m-{run}"), "role": "assistant"}),
        json!({"type": "TEXT_MESSAGE_CONTENT", "messageId": format!("m-{run}"), "delta": text}),
        json!({"type": "TEXT_MESSAGE_END", "messageId": format!("m-{run}")}),
        json!({"type": "RUN_FINISHED", "threadId": thread, "runId": run}),
    ]
}

/// The standard script: [`run`] for `t1`/`r1`.
pub fn events() -> Vec<Value> {
    run("t1", "r1", "Hello")
}

/// SSE text: one `data:` line per event, LF line endings.
pub fn sse(events: &[Value]) -> Vec<u8> {
    let mut out = Vec::new();
    for e in events {
        out.extend_from_slice(b"data: ");
        out.extend_from_slice(e.to_string().as_bytes());
        out.extend_from_slice(b"\n\n");
    }
    out
}

/// Length-prefixed protobuf frames, one per event.
#[cfg(feature = "proto")]
pub fn proto(events: &[Value]) -> Vec<u8> {
    let mut out = Vec::new();
    for e in events {
        let mut message = Vec::new();
        agui_sdk::proto::encode::encode_value(e, &mut message)
            .unwrap_or_else(|err| panic!("encode {e}: {err}"));
        agui_sdk::proto::framing::write_frame(&message, &mut out)
            .unwrap_or_else(|err| panic!("frame: {err}"));
    }
    out
}

/// Every frame of `stream` as JSON, and the error that ended it, if any.
pub async fn drain(mut stream: FrameStream) -> (Vec<Value>, Option<Error>) {
    let mut frames = Vec::new();
    while let Some(item) = stream.next().await {
        match item {
            Ok(frame) => frames.push(
                frame
                    .into_value()
                    .unwrap_or_else(|e| panic!("frame is not JSON: {e}")),
            ),
            Err(e) => return (frames, Some(e)),
        }
    }
    (frames, None)
}

/// Runs frames through the client pipeline; the verdict at end of stream.
pub fn judge(frames: &[Value]) -> Result<Termination, Error> {
    let sink = Arc::new(CollectingSink::new());
    let mut reporter = Reporter::new(sink, 256);
    let mut processor = Processor::new(PipelineConfig::default());
    let mut out = Vec::new();
    for frame in frames {
        processor.push_value(frame.clone(), &mut reporter, &mut out)?;
    }
    Ok(processor.finish())
}

/// The event types of `frames`, in order.
pub fn types(frames: &[Value]) -> Vec<String> {
    frames
        .iter()
        .map(|f| f["type"].as_str().unwrap_or("?").to_owned())
        .collect()
}

/// Panics with context unless `result` is an error with `code`.
#[track_caller]
pub fn expect_code<T: std::fmt::Debug>(
    result: Result<T, Error>,
    code: agui_sdk::ErrorCode,
) -> Error {
    match result {
        Ok(v) => panic!("expected {code:?}, got Ok({v:?})"),
        Err(e) => {
            assert_eq!(e.code(), code, "{e}");
            e
        }
    }
}
