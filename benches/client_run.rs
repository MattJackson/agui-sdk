//! `AgentClient` over an in-memory transport: the whole client run loop
//! (frame stream, processor, update delivery, observer, report) on the
//! typical stream. Budget: `pipeline_push_json/text_heavy`'s allocations per
//! event plus 0, with or without an observer; updates are an `Arc` clone and
//! views borrow. Prints updates per second.

mod support;

use std::future::{Future, IntoFuture};
use std::sync::Arc;
use std::task::{Context, Poll, Wake, Waker};
use std::time::Instant;

use agui_sdk::client::transport::TransportRequest;
use agui_sdk::client::{
    AgentClient, Frame, FrameStream, Observer, RunRequest, RunUpdate, RunView, Transport,
};
use agui_sdk::diagnostics::NullSink;
use agui_sdk::wire::Chunk;
use support::{typical_stream, Bench, CountingAllocator, Work};

#[global_allocator]
static ALLOC: CountingAllocator = CountingAllocator;

struct Noop;

impl Wake for Noop {
    fn wake(self: Arc<Self>) {}
}

/// The in-memory transport never returns `Pending` except for the client's
/// cooperative yield, which wakes at once: a busy poll loop is enough.
fn block_on<F: Future>(future: F) -> F::Output {
    let waker = Waker::from(Arc::new(Noop));
    let mut cx = Context::from_waker(&waker);
    let mut future = std::pin::pin!(future);
    loop {
        if let Poll::Ready(out) = future.as_mut().poll(&mut cx) {
            return out;
        }
    }
}

/// Serves pre-serialized frames; cloning a `'static` chunk allocates nothing.
#[derive(Clone)]
struct InMemory(Arc<Vec<Chunk>>);

impl Transport for InMemory {
    async fn open(&self, _request: TransportRequest) -> agui_sdk::Result<FrameStream> {
        let frames = Arc::clone(&self.0);
        let mut next = 0;
        Ok(FrameStream::from_poll_fn(move |_cx| {
            let frame = frames.get(next).cloned().map(|c| Ok(Frame::from_json(c)));
            next += 1;
            Poll::Ready(frame)
        }))
    }
}

struct Reader;

impl Observer for Reader {
    fn on_event(&self, update: &RunUpdate, view: RunView<'_>) {
        std::hint::black_box((
            update.event.event_type(),
            view.messages().len(),
            view.state(),
        ));
    }
}

fn setup(runs: usize) -> (Arc<Vec<Chunk>>, u64, u64, String) {
    let events = typical_stream(runs);
    let last_run = format!("r-{}", runs - 1);
    let chunks: Vec<Chunk> = events
        .iter()
        .map(|e| {
            let bytes: &'static [u8] =
                Box::leak(serde_json::to_vec(e).unwrap_or_default().into_boxed_slice());
            Chunk::from(bytes)
        })
        .collect();
    let bytes = chunks.iter().map(|c| c.len() as u64).sum();
    (Arc::new(chunks), events.len() as u64, bytes, last_run)
}

fn client(chunks: &Arc<Vec<Chunk>>, observer: bool) -> AgentClient<InMemory> {
    let mut builder =
        AgentClient::with_transport(InMemory(Arc::clone(chunks))).diagnostic_sink(NullSink);
    if observer {
        builder = builder.observer(Reader);
    }
    builder.build().unwrap_or_else(|e| panic!("{e}"))
}

fn main() {
    let mut bench = Bench::new("client_run");
    bench.case(
        "await",
        300,
        |runs| {
            let (chunks, events, bytes, last) = setup(runs);
            let client = client(&chunks, false);
            let prepare = move || RunRequest::new().thread_id("t-bench").run_id(last.as_str());
            let run = move |request: RunRequest| {
                let report = block_on(client.run(request).into_future());
                assert!(report.unwrap_or_else(|e| panic!("{e}")).status.is_success());
                Work { events, bytes }
            };
            (prepare, run)
        },
        None,
    );
    bench.case(
        "iterate_observed",
        300,
        |runs| {
            let (chunks, events, bytes, last) = setup(runs);
            let client = client(&chunks, true);
            let prepare = move || RunRequest::new().thread_id("t-bench").run_id(last.as_str());
            let run = move |request: RunRequest| {
                let mut handle = client.run(request);
                block_on(async {
                    while let Some(update) = handle.next().await {
                        std::hint::black_box(update.unwrap_or_else(|e| panic!("{e}")));
                    }
                });
                Work { events, bytes }
            };
            (prepare, run)
        },
        None,
    );

    // Updates per second, for the record (the gate above judges allocations).
    let (chunks, events, _, last) = setup(300);
    let client = client(&chunks, false);
    let passes = if std::env::args().any(|a| a == "--bench") {
        20
    } else {
        1
    };
    let start = Instant::now();
    for _ in 0..passes {
        let request = RunRequest::new().thread_id("t-bench").run_id(last.as_str());
        let _ = block_on(client.run(request).into_future());
    }
    let per_second = (events * passes) as f64 / start.elapsed().as_secs_f64();
    println!("client_run: {per_second:.0} updates/s over the in-memory transport");
    bench.finish();
}
