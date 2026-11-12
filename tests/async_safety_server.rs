//! Server-side async safety: a panicking diagnostic sink, a send dropped
//! while it waits for queue space, and what cancellation does to the agent's
//! future. Each run still ends with exactly one terminal event.

#![cfg(feature = "server")]
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support_server;

use std::collections::HashSet;
use std::future::{poll_fn, Future};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::task::Poll;

use agui_sdk::diagnostics::{Diagnostic, DiagnosticSink};
use agui_sdk::event::Event;
use agui_sdk::server::input::{admit, InputPolicy};
use agui_sdk::server::{CancelToken, Extensions, RunOutcome, Runner, RunnerConfig, Transformer};
use agui_sdk::text::{TextMessageContent, TextMessageEnd, TextMessageStart};
use agui_sdk::version::LegacyEra;
use agui_sdk::{ErrorCode, EventType, Message, RunAgentInput};
use serde_json::json;

use support_server::*;

fn panicking_sink(_: &Diagnostic) {
    panic!("the application's diagnostic sink panicked");
}

#[test]
fn a_sink_panic_during_admission_reporting_still_ends_the_run_once() {
    // An unknown member is stripped with a warning at admission (RUNIN-29);
    // the runner reports it to the sink before the run opens.
    let body = br#"{"threadId":"t1","runId":"r1","messages":[],"tools":[],"context":[],
        "protocolVersion":"1.0","notInTheSchema":1}"#;
    let admitted = admit(body, &InputPolicy::default()).unwrap();
    assert!(
        !admitted.diagnostics.is_empty(),
        "the input draws a diagnostic"
    );
    let runner = Runner::new(agent(|_ctx| async move { Ok(RunOutcome::success()) }))
        .diagnostic_sink(panicking_sink);
    let stream = runner
        .run(admitted, CancelToken::new(), Extensions::new())
        .unwrap();
    let events = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drain(stream)))
        .expect("the sink's panic must not escape the stream");
    assert_one_valid_run(&events);
    let error = run_error(&events);
    assert_eq!(
        error.code.as_deref(),
        Some(ErrorCode::AgentPanicked.as_str())
    );
}

#[test]
fn a_sink_panic_mid_run_fails_the_run_with_one_terminal() {
    // A legacy peer: the subagent event cannot be expressed in its era, so
    // the downgrade drops it with a lossy-downgrade warning to the sink.
    let mut config = RunnerConfig::default();
    config.legacy_peer = Some(LegacyEra::V0_0_39);
    let runner = Runner::new(agent(|ctx| async move {
        let scope = ctx.emitter().start_subagent("helper").await?;
        scope.emitter().say("hello").await?;
        scope.finish(None, None).await?;
        ctx.emitter().say("done").await?;
        Ok(RunOutcome::success())
    }))
    .config(config)
    .diagnostic_sink(panicking_sink);
    let input = RunAgentInput::new("t1", "r1", vec![Message::user("hi")]);
    let stream = start(&runner, input, CancelToken::new());
    let events = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drain(stream)))
        .expect("the sink's panic must not escape the stream");
    assert_one_valid_run(&events);
    let error = run_error(&events);
    assert_eq!(
        error.code.as_deref(),
        Some(ErrorCode::AgentPanicked.as_str())
    );
}

/// A sink whose `enabled` panics: asked when the run's reporter is created.
struct PanickingEnabled;

impl DiagnosticSink for PanickingEnabled {
    fn report(&self, _: &Diagnostic) {}

    fn enabled(&self) -> bool {
        panic!("the application's diagnostic sink panicked in enabled");
    }
}

#[test]
fn a_sink_panic_in_enabled_fails_the_run_with_one_terminal() {
    let runner = Runner::new(agent(|_ctx| async move { Ok(RunOutcome::success()) }))
        .diagnostic_sink(PanickingEnabled);
    let input = RunAgentInput::new("t1", "r1", vec![Message::user("hi")]);
    let events = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        drain(start(&runner, input, CancelToken::new()))
    }))
    .expect("the sink's panic must not escape the runner");
    assert_one_valid_run(&events);
    let error = run_error(&events);
    assert_eq!(
        error.code.as_deref(),
        Some(ErrorCode::AgentPanicked.as_str())
    );
}

/// Opens a message on its first content, as a provider translator would.
#[derive(Default)]
struct OpenOnFirstContent {
    open: HashSet<String>,
}

impl Transformer for OpenOnFirstContent {
    fn transform(&mut self, event: Event, out: &mut Vec<Event>) {
        if let Event::TextMessageContent(content) = &event {
            if self.open.insert(content.message_id.as_str().to_owned()) {
                out.push(TextMessageStart::new(content.message_id.clone()).into());
            }
        }
        out.push(event);
    }
}

#[test]
fn a_send_dropped_while_waiting_for_space_is_committed_before_the_next() {
    let mut config = RunnerConfig::default();
    config.queue_capacity = 2;
    let runner = Runner::new(agent(|ctx| async move {
        let emitter = ctx.emitter().clone();
        // One of the two slots taken.
        emitter.custom("first", json!(1)).await?;
        {
            // The transformer turns this into START + CONTENT: two events,
            // one free slot, so the send waits; it is then dropped.
            let mut send = Box::pin(emitter.emit(TextMessageContent::new("m1", "a")));
            let pending = poll_fn(|cx| Poll::Ready(send.as_mut().poll(cx).is_pending())).await;
            assert!(pending, "the send should be waiting for queue space");
        }
        // The transformer already opened m1, so it sends no START now.
        emitter.emit(TextMessageContent::new("m1", "b")).await?;
        emitter.emit(TextMessageEnd::new("m1")).await?;
        Ok(RunOutcome::success())
    }))
    .config(config)
    .diagnostic_sink(agui_sdk::diagnostics::NullSink)
    .transformer(OpenOnFirstContent::default);
    let events = run(&runner, input());
    assert_one_valid_run(&events);
    assert_eq!(
        types(&events),
        [
            EventType::RunStarted,
            EventType::Custom,
            EventType::TextMessageStart,
            EventType::TextMessageContent,
            EventType::TextMessageContent,
            EventType::TextMessageEnd,
            EventType::RunFinished,
        ]
    );
}

/// Sets its flag when dropped.
struct DropFlag(Arc<AtomicBool>);

impl Drop for DropFlag {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

#[test]
fn cancellation_drops_the_agent_future_and_runs_its_destructors() {
    let resumed = Arc::new(AtomicBool::new(false));
    let dropped = Arc::new(AtomicBool::new(false));
    let (resumed_in, dropped_in) = (resumed.clone(), dropped.clone());
    let runner = Runner::new(agent(move |ctx| {
        let resumed = resumed_in.clone();
        let guard = DropFlag(dropped_in.clone());
        async move {
            let _guard = guard;
            ctx.emitter().custom("waiting", json!(null)).await?;
            ctx.cancellation().cancelled().await;
            // Documented: never reached; the runner drops the future.
            resumed.store(true, Ordering::SeqCst);
            Ok(RunOutcome::success())
        }
    }))
    .diagnostic_sink(agui_sdk::diagnostics::NullSink);
    let cancel = CancelToken::new();
    let mut stream = start(&runner, input(), cancel.clone());
    let events = futures_executor::block_on(async {
        // RUN_STARTED, then the agent's event: it is now suspended.
        let mut out = vec![stream.next().await.unwrap(), stream.next().await.unwrap()];
        cancel.cancel();
        while let Some(event) = stream.next().await {
            out.push(event);
        }
        out
    });
    assert_one_valid_run(&events);
    assert_eq!(
        finished(&events).outcome,
        Some(agui_sdk::lifecycle::RunFinishedOutcome::cancelled())
    );
    assert!(
        dropped.load(Ordering::SeqCst),
        "the agent future was dropped"
    );
    assert!(
        !resumed.load(Ordering::SeqCst),
        "the agent was not polled again"
    );
}

/// Sets its flag, then panics, when dropped.
struct PanicOnDrop(Arc<AtomicBool>);

impl Drop for PanicOnDrop {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
        panic!("the agent's guard panicked while dropped");
    }
}

#[test]
fn an_agent_destructor_panic_on_disconnect_does_not_escape_the_stream_drop() {
    let dropped = Arc::new(AtomicBool::new(false));
    let dropped_in = dropped.clone();
    let runner = Runner::new(agent(move |ctx| {
        let guard = PanicOnDrop(dropped_in.clone());
        async move {
            let _guard = guard;
            ctx.emitter().custom("waiting", json!(null)).await?;
            ctx.cancellation().cancelled().await;
            Ok(RunOutcome::success())
        }
    }))
    .diagnostic_sink(agui_sdk::diagnostics::NullSink);
    let mut stream = start(&runner, input(), CancelToken::new());
    let seen = futures_executor::block_on(async {
        // RUN_STARTED, then the agent's event: it is now suspended.
        vec![stream.next().await.unwrap(), stream.next().await.unwrap()]
    });
    assert_eq!(types(&seen), [EventType::RunStarted, EventType::Custom]);
    // The client disconnects: the stream is dropped mid-run.
    let dropped_cleanly = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(stream)));
    assert!(
        dropped_cleanly.is_ok(),
        "the agent destructor's panic escaped the stream's drop"
    );
    assert!(
        dropped.load(Ordering::SeqCst),
        "the agent future was dropped"
    );
}

/// Polls a send once from an agent destructor and records how it ended:
/// `Some(Err)` is the disconnect the stream's drop must report, `Some(Ok)`
/// or `None` (waiting for space, which would block a destructor that waits
/// on its sender) mean the run was still open.
struct SendOnDrop(
    Option<agui_sdk::server::emit::Emitter>,
    Arc<std::sync::Mutex<Option<bool>>>,
);

impl Drop for SendOnDrop {
    fn drop(&mut self) {
        struct Noop;
        impl std::task::Wake for Noop {
            fn wake(self: Arc<Self>) {}
        }
        let emitter = self.0.take().unwrap();
        let waker = std::task::Waker::from(Arc::new(Noop));
        let mut cx = std::task::Context::from_waker(&waker);
        let mut send = Box::pin(emitter.custom("late", json!(null)));
        let failed = match send.as_mut().poll(&mut cx) {
            Poll::Ready(Err(_)) => Some(true),
            Poll::Ready(Ok(())) | Poll::Pending => Some(false),
        };
        *self.1.lock().unwrap() = failed;
    }
}

#[test]
fn a_dropped_stream_is_closed_before_agent_destructors_run() {
    let seen = Arc::new(std::sync::Mutex::new(None));
    let seen_in = seen.clone();
    let runner = Runner::new(agent(move |ctx| {
        let guard = SendOnDrop(Some(ctx.emitter().clone()), seen_in.clone());
        async move {
            let _guard = guard;
            ctx.emitter().custom("waiting", json!(null)).await?;
            ctx.cancellation().cancelled().await;
            Ok(RunOutcome::success())
        }
    }))
    .diagnostic_sink(agui_sdk::diagnostics::NullSink);
    let mut stream = start(&runner, input(), CancelToken::new());
    futures_executor::block_on(async {
        stream.next().await.unwrap();
        stream.next().await.unwrap();
    });
    // The client disconnects: the stream is dropped mid-run.
    drop(stream);
    assert_eq!(
        *seen.lock().unwrap(),
        Some(true),
        "a send from an agent destructor must fail as a disconnect"
    );
}
