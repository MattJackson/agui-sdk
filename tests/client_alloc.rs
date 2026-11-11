//! Allocation budget of the client run loop : per event,
//! the client adds nothing to what the pipeline allocates. Updates hand out
//! the shared `Arc<Event>`, views borrow, and the history is never cloned
//! per event.

#![cfg(feature = "client")]
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support_client;

use std::future::IntoFuture;
use std::sync::Arc;

use agui_sdk::client::transport::TransportRequest;
use agui_sdk::client::{
    AgentClient, Frame, FrameStream, Observer, RunRequest, RunUpdate, RunView, Transport,
};
use agui_sdk::diagnostics::{NullSink, Reporter};
use agui_sdk::pipeline::{PipelineConfig, Processor};
use agui_sdk::wire::Chunk;
use agui_test_support::alloc::{self, CountingAllocator};
use serde_json::{json, Value};
use support_client::block_on;

#[global_allocator]
static ALLOC: CountingAllocator = CountingAllocator;

/// `runs` complete runs on one stream, each a text message of 20 deltas
/// and a state delta.
fn stream(runs: usize) -> Vec<Value> {
    let mut out = Vec::new();
    for r in 0..runs {
        let run = format!("r{r}");
        let m = format!("m{r}");
        out.push(json!({"type": "RUN_STARTED", "threadId": "t1", "runId": if r + 1 == runs { "last".to_owned() } else { run.clone() }}));
        out.push(json!({"type": "TEXT_MESSAGE_START", "messageId": m, "role": "assistant"}));
        for i in 0..20 {
            out.push(json!({"type": "TEXT_MESSAGE_CONTENT", "messageId": m, "delta": format!("tok{i} ")}));
        }
        out.push(json!({"type": "TEXT_MESSAGE_END", "messageId": m}));
        out.push(json!({"type": "STATE_SNAPSHOT", "snapshot": {"n": r}}));
        out.push(json!({"type": "RUN_FINISHED", "threadId": "t1", "runId": if r + 1 == runs { "last".to_owned() } else { run }}));
    }
    out
}

/// Frames whose bytes are `'static`, so cloning them allocates nothing.
fn frames(events: &[Value]) -> Vec<Chunk> {
    events
        .iter()
        .map(|e| {
            let bytes: &'static [u8] = Box::leak(serde_json::to_vec(e).unwrap().into_boxed_slice());
            Chunk::from(bytes)
        })
        .collect()
}

#[derive(Clone)]
struct InMemory(Arc<Vec<Chunk>>);

impl Transport for InMemory {
    async fn open(&self, _request: TransportRequest) -> agui_sdk::Result<FrameStream> {
        let frames = Arc::clone(&self.0);
        let mut next = 0;
        Ok(FrameStream::from_poll_fn(move |_cx| {
            let frame = frames.get(next).cloned().map(|c| Ok(Frame::from_json(c)));
            next += 1;
            std::task::Poll::Ready(frame)
        }))
    }
}

struct Counting;

impl Observer for Counting {
    fn on_event(&self, update: &RunUpdate, view: RunView<'_>) {
        std::hint::black_box((
            update.event.event_type(),
            view.messages().len(),
            view.state(),
        ));
    }
}

fn pipeline_allocs(chunks: &[Chunk]) -> u64 {
    let (_, stats) = alloc::measure(|| {
        let mut processor = Processor::new(PipelineConfig::default());
        let mut reporter = Reporter::new(Arc::new(NullSink), 256);
        let mut out = Vec::new();
        for chunk in chunks {
            out.clear();
            processor.push_json(chunk, &mut reporter, &mut out).unwrap();
        }
    });
    stats.total()
}

fn client_allocs(chunks: &[Chunk], observer: bool) -> u64 {
    let mut builder =
        AgentClient::with_transport(InMemory(Arc::new(chunks.to_vec()))).diagnostic_sink(NullSink);
    if observer {
        builder = builder.observer(Counting);
    }
    let client = builder.build().unwrap();
    let request = || RunRequest::new().thread_id("t1").run_id("last");
    // Warm-up (one-time statics).
    block_on(client.run(request()).into_future()).unwrap();
    let (report, stats) = alloc::measure(|| block_on(client.run(request()).into_future()));
    assert!(report.unwrap().status.is_success());
    stats.total()
}

#[test]
fn the_client_adds_no_allocation_per_event() {
    if !alloc::installed() {
        eprintln!("counting allocator not installed; skipping");
        return;
    }
    let small = stream(4);
    let large = stream(12);
    let (fs, fl) = (frames(&small), frames(&large));
    let extra_events = (large.len() - small.len()) as u64;

    let pipeline = pipeline_allocs(&fl) - pipeline_allocs(&fs);
    for observer in [false, true] {
        let client = client_allocs(&fl, observer) - client_allocs(&fs, observer);
        // Exactly equal in practice; a few allocations of slack absorb
        // one-time thread-local initialisation landing in either
        // measurement. Anything per event would show as hundreds.
        assert!(
            client <= pipeline + 4,
            "marginal allocations over {extra_events} events: client {client} vs pipeline {pipeline} (observer: {observer})"
        );
    }
}
