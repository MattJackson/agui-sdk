//! Client-side async safety: a panicking diagnostic sink, a panicking custom
//! transport, a cancel that arrives after the run's terminal was received,
//! and a `Thread::run_and_absorb` dropped mid-run.

#![cfg(feature = "client")]
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support_client;

use std::future::{Future, IntoFuture};
use std::pin::pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Wake, Waker};

use agui_sdk::client::middleware::{MiddlewareRequest, Next};
use agui_sdk::client::transport::TransportRequest;
use agui_sdk::client::{
    AgentClient, FrameStream, Observer, RunInfo, RunReport, RunRequest, Transport,
};
use agui_sdk::diagnostics::{Diagnostic, NullSink};
use agui_sdk::event::EventType;
use agui_sdk::Error;
use agui_sdk::ErrorCode;
use serde_json::json;
use support_client::*;

fn request() -> RunRequest {
    RunRequest::new().thread_id("t1").run_id("r1").user("hi")
}

#[test]
fn a_panicking_diagnostic_sink_fails_the_run_instead_of_unwinding() {
    let frames = vec![
        run_started("t1", "r1"),
        // Unknown to 1.0: skipped with a diagnostic (to the sink).
        json!({"type": "SOMETHING_NEW"}),
        run_finished("t1", "r1"),
    ];
    let client = AgentClient::with_transport(mock(frames))
        .diagnostic_sink(|_: &Diagnostic| panic!("the application's sink panicked"))
        .build()
        .unwrap();
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        block_on(client.run(request()).into_future())
    }))
    .expect("the sink's panic must not escape the run");
    assert_eq!(outcome.unwrap_err().code(), ErrorCode::ObserverPanicked);
}

/// A custom transport that panics while opening (`open_panics`) or on the
/// first read of the stream it returns.
struct PanickingTransport {
    open_panics: bool,
}

impl Transport for PanickingTransport {
    async fn open(&self, _request: TransportRequest) -> agui_sdk::Result<FrameStream> {
        if self.open_panics {
            panic!("the custom transport panicked while opening");
        }
        Ok(FrameStream::from_poll_fn(|_cx| {
            panic!("the custom transport panicked while reading")
        }))
    }
}

/// Records the error code of every `on_finish`.
#[derive(Clone, Default)]
struct Finishes(Arc<Mutex<Vec<Option<ErrorCode>>>>);

impl Observer for Finishes {
    fn on_finish(&self, result: Result<&RunReport, &Error>, _run: RunInfo<'_>) {
        self.0.lock().unwrap().push(result.err().map(Error::code));
    }
}

#[test]
fn a_panicking_custom_transport_fails_the_run_instead_of_unwinding() {
    for open_panics in [true, false] {
        for with_middleware in [false, true] {
            let case = format!("open_panics={open_panics} with_middleware={with_middleware}");
            let finishes = Finishes::default();
            let mut builder = AgentClient::with_transport(PanickingTransport { open_panics })
                .diagnostic_sink(NullSink)
                .observer(finishes.clone());
            if with_middleware {
                builder = builder.middleware(|r: MiddlewareRequest, n: Next| n.run(r));
            }
            let client = builder.build().unwrap();
            let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                block_on(client.run(request()).into_future())
            }));
            let err = outcome
                .unwrap_or_else(|_| panic!("{case}: the transport's panic escaped the run"))
                .expect_err(&case);
            assert_eq!(err.code(), ErrorCode::StreamRead, "{case}: {err}");
            assert!(
                err.to_string().contains("the transport panicked"),
                "{case}: {err}"
            );
            assert_eq!(
                *finishes.0.lock().unwrap(),
                [Some(ErrorCode::StreamRead)],
                "{case}: exactly one on_finish, with the transport's failure"
            );
        }
    }
}

#[test]
fn a_cancel_after_the_terminal_was_received_does_not_override_it() {
    let client = AgentClient::with_transport(mock(run_of("t1", "r1", text("m1", "Hey"))))
        .diagnostic_sink(NullSink)
        .build()
        .unwrap();
    let mut run = client.run(request());
    // Read up to and including RUN_FINISHED, then press "stop" before the
    // stream is read to its end.
    loop {
        let update = block_on(run.next()).unwrap().unwrap();
        if update.event.event_type() == EventType::RunFinished {
            break;
        }
    }
    run.cancel();
    let report = block_on(run.into_future()).expect("the server's terminal wins");
    assert!(report.status.is_success());
    assert_eq!(report.messages.last().unwrap().text(), Some("Hey"));
}

#[test]
fn a_cancel_while_parked_after_the_terminal_concludes_as_the_terminal_says() {
    // The transport delivers the whole run, then stalls without closing.
    let transport = StallingTransport {
        frames: vec![run_started("t1", "r1"), run_finished("t1", "r1")],
        ..Default::default()
    };
    let client = AgentClient::with_transport(transport)
        .diagnostic_sink(NullSink)
        .build()
        .unwrap();
    let mut run = client.run(request());
    let mut seen = Vec::new();
    for _ in 0..2 {
        seen.push(block_on(run.next()).unwrap().unwrap().event.event_type());
    }
    assert_eq!(seen, [EventType::RunStarted, EventType::RunFinished]);
    run.cancel();
    let report = block_on(run.into_future()).expect("a received RUN_FINISHED wins");
    assert!(report.status.is_success());
}

struct Noop;

impl Wake for Noop {
    fn wake(self: Arc<Self>) {}
}

#[test]
fn dropping_run_and_absorb_mid_run_leaves_the_thread_usable() {
    let transport = StallingTransport {
        frames: vec![run_started("t1", "r1")],
        ..Default::default()
    };
    let client = AgentClient::with_transport(transport.clone())
        .diagnostic_sink(NullSink)
        .build()
        .unwrap();
    let mut thread = client.thread_with_id("t1");
    let staged_history = thread.messages().len();
    {
        let waker = Waker::from(Arc::new(Noop));
        let mut cx = Context::from_waker(&waker);
        let mut future = pin!(thread.run_and_absorb(RunRequest::new().run_id("r1").user("hi")));
        assert!(future.as_mut().poll(&mut cx).is_pending());
        // Dropped here, as a timeout or a lost `select!` branch would.
    }
    assert_eq!(
        thread.messages().len(),
        staged_history,
        "nothing was absorbed"
    );
    // Not wedged: the next run is accepted.
    let next = thread.run(RunRequest::new().run_id("r2").user("again"));
    assert!(next.is_ok(), "{:?}", next.err());
}
