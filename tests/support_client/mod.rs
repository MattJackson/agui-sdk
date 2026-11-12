//! Helpers for the client tests: a std-only `block_on`, event builders and
//! two in-memory transports (scripted frames are `agui_test_support`'s
//! `MockTransport`; these add a stream that stalls, for cancellation, and
//! one whose frames are parsed values).

#![allow(dead_code, clippy::unwrap_used, clippy::expect_used)]

use std::future::Future;
use std::pin::pin;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Wake, Waker};
use std::thread::{self, Thread};

use agui_sdk::client::transport::TransportRequest;
use agui_sdk::client::{Frame, FrameStream, Transport};
use agui_sdk::wire::Chunk;
use serde_json::{json, Value};

pub use agui_test_support::mock::{Delivery, MockTransport};

struct Unpark(Thread);

impl Wake for Unpark {
    fn wake(self: Arc<Self>) {
        self.0.unpark();
    }
}

/// Drives a future to completion on the current thread.
pub fn block_on<F: Future>(future: F) -> F::Output {
    let waker = Waker::from(Arc::new(Unpark(thread::current())));
    let mut cx = Context::from_waker(&waker);
    let mut future = pin!(future);
    loop {
        match future.as_mut().poll(&mut cx) {
            Poll::Ready(output) => return output,
            Poll::Pending => thread::park(),
        }
    }
}

/// A scripted transport serving `frames` as raw JSON frames.
pub fn mock(frames: Vec<Value>) -> MockTransport {
    MockTransport::new(frames, Delivery::Frames)
}

pub fn run_started(thread: &str, run: &str) -> Value {
    json!({"type": "RUN_STARTED", "threadId": thread, "runId": run})
}

pub fn run_finished(thread: &str, run: &str) -> Value {
    json!({"type": "RUN_FINISHED", "threadId": thread, "runId": run})
}

pub fn text(message: &str, delta: &str) -> Vec<Value> {
    vec![
        json!({"type": "TEXT_MESSAGE_START", "messageId": message, "role": "assistant"}),
        json!({"type": "TEXT_MESSAGE_CONTENT", "messageId": message, "delta": delta}),
        json!({"type": "TEXT_MESSAGE_END", "messageId": message}),
    ]
}

pub fn tool_call(id: &str, name: &str, args: &str, parent: &str) -> Vec<Value> {
    vec![
        json!({"type": "TOOL_CALL_START", "toolCallId": id, "toolCallName": name, "parentMessageId": parent}),
        json!({"type": "TOOL_CALL_ARGS", "toolCallId": id, "delta": args}),
        json!({"type": "TOOL_CALL_END", "toolCallId": id}),
    ]
}

/// A complete run: started, `body`, finished.
pub fn run_of(thread: &str, run: &str, body: Vec<Value>) -> Vec<Value> {
    let mut out = vec![run_started(thread, run)];
    out.extend(body);
    out.push(run_finished(thread, run));
    out
}

/// Serves `frames` (as raw JSON), then stalls until the run's cancel token
/// fires; counts opens. `stalled` is set once every frame has been handed
/// over and the stream first parks, so a test can cancel after delivery
/// without relying on wall-clock timing.
#[derive(Clone, Default)]
pub struct StallingTransport {
    pub frames: Vec<Value>,
    pub opens: Arc<AtomicUsize>,
    pub seen_cancel: Arc<Mutex<Option<agui_sdk::client::CancelToken>>>,
    pub stalled: Arc<AtomicBool>,
}

impl Transport for StallingTransport {
    async fn open(&self, request: TransportRequest) -> agui_sdk::Result<FrameStream> {
        self.opens.fetch_add(1, Ordering::SeqCst);
        *self.seen_cancel.lock().unwrap() = Some(request.cancel.clone());
        let mut frames = self
            .frames
            .iter()
            .map(|f| Frame::from_json(Chunk::from(serde_json::to_vec(f).unwrap())))
            .collect::<Vec<_>>()
            .into_iter();
        let stalled = Arc::clone(&self.stalled);
        Ok(FrameStream::from_poll_fn(move |_cx| match frames.next() {
            Some(frame) => Poll::Ready(Some(Ok(frame))),
            // Never wakes: only cancellation can end the run.
            None => {
                stalled.store(true, Ordering::SeqCst);
                Poll::Pending
            }
        }))
    }
}

/// Serves already-parsed frames (`Frame::from_value`), as a transport that
/// decodes protobuf does.
#[derive(Clone)]
pub struct ValueTransport(pub Vec<Value>);

impl Transport for ValueTransport {
    async fn open(&self, _request: TransportRequest) -> agui_sdk::Result<FrameStream> {
        let frames: Vec<_> = self
            .0
            .iter()
            .cloned()
            .map(|v| Ok(Frame::from_value(v)))
            .collect();
        Ok(FrameStream::from_frames(frames))
    }
}

/// Serves `runs[n]` on the n-th open (the last script again after that):
/// a thread's successive runs, each with its own stream. Frames are raw JSON
/// (`Frame::from_json`, the no-middleware fast path) or, with `parsed`,
/// already-parsed values (`Frame::from_value`, as a protobuf decoder hands
/// them on).
#[derive(Clone)]
pub struct RunScripts {
    pub runs: Arc<Vec<Vec<Value>>>,
    pub opens: Arc<AtomicUsize>,
    pub parsed: bool,
}

impl RunScripts {
    pub fn json(runs: Vec<Vec<Value>>) -> Self {
        RunScripts {
            runs: Arc::new(runs),
            opens: Arc::new(AtomicUsize::new(0)),
            parsed: false,
        }
    }

    pub fn parsed(runs: Vec<Vec<Value>>) -> Self {
        RunScripts {
            parsed: true,
            ..Self::json(runs)
        }
    }
}

impl Transport for RunScripts {
    async fn open(&self, _request: TransportRequest) -> agui_sdk::Result<FrameStream> {
        let n = self.opens.fetch_add(1, Ordering::SeqCst);
        let script = &self.runs[n.min(self.runs.len() - 1)];
        let frames: Vec<_> = script
            .iter()
            .map(|v| {
                Ok(if self.parsed {
                    Frame::from_value(v.clone())
                } else {
                    Frame::from_json(Chunk::from(serde_json::to_vec(v).unwrap()))
                })
            })
            .collect();
        Ok(FrameStream::from_frames(frames))
    }
}
