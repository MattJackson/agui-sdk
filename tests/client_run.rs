//! The client run surface over in-memory transports: statuses, reports,
//! partial reports, cancellation, the drop guard, observers, middleware,
//! subscribers and the frame guard (client half of the lifecycle and
//! transport contracts; the verdicts themselves are the pipeline's).

#![cfg(feature = "client")]
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support_client;

use std::future::IntoFuture;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use agui_sdk::client::middleware::{map_frames, FrameStream, MiddlewareRequest, Next};
use agui_sdk::client::subscriber::{Flow, Subscriber};
use agui_sdk::client::{
    AgentClient, CancelToken, Frame, Observer, RunInfo, RunReport, RunRequest, RunStatus,
    RunUpdate, RunView,
};
use agui_sdk::diagnostics::{CollectingSink, Diagnostic, DiagnosticCode, NullSink};
use agui_sdk::event::{Event, EventType};
use agui_sdk::lifecycle::RunFinishedOutcome;
use agui_sdk::pipeline::Limits;
use agui_sdk::{Error, ErrorCode, ErrorKind, Message};
use serde_json::{json, Value};
use support_client::*;

fn client(frames: Vec<Value>) -> AgentClient<MockTransport> {
    AgentClient::with_transport(mock(frames))
        .diagnostic_sink(NullSink)
        .build()
        .unwrap()
}

fn request() -> RunRequest {
    RunRequest::new().thread_id("t1").run_id("r1").user("hi")
}

#[test]
fn a_complete_run_reports_history_state_and_status() {
    let mut body = text("m1", "Hello");
    body.push(json!({"type": "STATE_SNAPSHOT", "snapshot": {"n": 1}}));
    let client = client(run_of("t1", "r1", body));
    let report = block_on(client.run(request()).into_future()).unwrap();
    assert!(report.status.is_success());
    assert_eq!(report.thread_id.as_str(), "t1");
    assert_eq!(report.run_id.as_str(), "r1");
    // The sent user message, then the assistant's.
    assert_eq!(report.messages.len(), 2);
    assert_eq!(report.messages[1].text(), Some("Hello"));
    assert_eq!(report.new_message_ids.len(), 1);
    assert_eq!(report.new_messages().next().unwrap().id().as_str(), "m1");
    assert_eq!(report.state, json!({"n": 1}));
    #[derive(serde::Deserialize)]
    struct S {
        n: u32,
    }
    assert_eq!(report.state_as::<S>().unwrap().n, 1);
    assert_eq!(
        report.state_as::<Vec<u8>>().unwrap_err().code(),
        ErrorCode::Json
    );
}

#[test]
fn the_handle_iterates_updates_and_borrows_the_view() {
    let client = client(run_of("t1", "r1", text("m1", "Hey")));
    let mut run = client.run(request());
    let mut types = Vec::new();
    block_on(async {
        while let Some(update) = run.next().await {
            let update = update.unwrap();
            types.push(update.event.event_type());
            if update.event.event_type() == EventType::TextMessageContent {
                assert_eq!(run.messages().last().unwrap().text(), Some("Hey"));
                let id = update.changes.message.clone().unwrap();
                assert_eq!(run.message(&id).unwrap().text(), Some("Hey"));
            }
        }
    });
    assert_eq!(
        types,
        [
            EventType::RunStarted,
            EventType::TextMessageStart,
            EventType::TextMessageContent,
            EventType::TextMessageEnd,
            EventType::RunFinished
        ]
    );
    // After the end the view is the report's.
    assert_eq!(run.messages().len(), 2);
    assert!(block_on(run.next()).is_none());
}

#[test]
fn result_is_reported_on_success_only() {
    let ok = vec![
        run_started("t1", "r1"),
        json!({"type": "RUN_FINISHED", "threadId": "t1", "runId": "r1", "result": {"answer": 42}}),
    ];
    let report = block_on(client(ok).run(request()).into_future()).unwrap();
    match report.status {
        RunStatus::Finished { result, .. } => assert_eq!(result, Some(json!({"answer": 42}))),
        other => panic!("{other:?}"),
    }
    let cancelled = vec![
        run_started("t1", "r1"),
        json!({"type": "RUN_FINISHED", "threadId": "t1", "runId": "r1", "result": 1, "outcome": {"type": "cancelled"}}),
    ];
    let report = block_on(client(cancelled).run(request()).into_future()).unwrap();
    assert!(report.status.is_cancelled());
    assert!(!report.status.is_success());
    assert!(matches!(
        report.status,
        RunStatus::Finished { result: None, .. }
    ));
}

#[test]
fn a_delivered_run_error_is_an_ok_report() {
    let frames = vec![
        run_started("t1", "r1"),
        json!({"type": "RUN_ERROR", "message": "boom", "code": "E1"}),
    ];
    let report = block_on(client(frames).run(request()).into_future()).unwrap();
    match &report.status {
        RunStatus::Errored(e) => assert_eq!(e.message, "boom"),
        other => panic!("{other:?}"),
    }
    assert!(!report.status.is_success());
}

#[test]
fn usage_and_interrupts_come_from_the_terminal_event() {
    let frames = vec![
        run_started("t1", "r1"),
        json!({"type": "RUN_FINISHED", "threadId": "t1", "runId": "r1",
               "usage": [{"provider": "openai", "inputTokens": 3}],
               "outcome": {"type": "interrupt", "interrupts": [{"id": "i1", "reason": "approval"}]}}),
    ];
    let report = block_on(client(frames).run(request()).into_future()).unwrap();
    assert!(report.status.is_interrupted());
    assert_eq!(report.usage.len(), 1);
    assert_eq!(report.interrupts[0].id.as_str(), "i1");
    assert!(matches!(
        report.status,
        RunStatus::Finished {
            outcome: Some(RunFinishedOutcome::Interrupt { .. }),
            result: None
        }
    ));
}

#[test]
fn truncation_is_an_error_with_a_partial_report_and_no_synthesized_finish() {
    let mut frames = vec![run_started("t1", "r1")];
    frames.extend(text("m1", "partial"));
    let client = client(frames);
    let mut run = client.run(request());
    let mut last = None;
    let err = block_on(async {
        loop {
            match run.next().await {
                Some(Ok(update)) => last = Some(update.event.event_type()),
                Some(Err(err)) => break err,
                None => panic!("a truncated run must end with an error"),
            }
        }
    });
    assert_eq!(last, Some(EventType::TextMessageEnd));
    assert_eq!(err.code(), ErrorCode::Truncated);
    assert_eq!(err.kind(), ErrorKind::Truncated);
    assert!(err.run_started());
    let partial = err.partial().unwrap();
    assert!(!partial.status.is_success());
    match &partial.status {
        RunStatus::Errored(e) => assert_eq!(e.code.as_deref(), Some("stream.truncated")),
        other => panic!("{other:?}"),
    }
    assert_eq!(partial.messages.last().unwrap().text(), Some("partial"));
    assert!(block_on(run.next()).is_none());
}

#[test]
fn a_failure_before_run_started_has_no_partial_report() {
    let transport = mock(vec![]).refusing(ErrorCode::HttpStatus, "503");
    let client = AgentClient::with_transport(transport).build().unwrap();
    let err = block_on(client.run(request()).into_future()).unwrap_err();
    assert_eq!(err.code(), ErrorCode::HttpStatus);
    assert!(!err.run_started());
    assert!(err.partial().is_none());
}

#[test]
fn a_rejected_stream_is_an_error_with_a_partial_report() {
    let frames = vec![
        run_started("t1", "r1"),
        json!({"type": "TEXT_MESSAGE_CONTENT", "messageId": "nope", "delta": "x"}),
    ];
    let err = block_on(client(frames).run(request()).into_future()).unwrap_err();
    assert_eq!(err.code(), ErrorCode::TextMessage);
    assert!(err.partial().is_some());
}

#[test]
fn nothing_is_sent_until_the_handle_is_polled() {
    let transport = mock(run_of("t1", "r1", vec![]));
    let client = AgentClient::with_transport(transport.clone())
        .build()
        .unwrap();
    let run = client.run(request());
    assert_eq!(transport.requests().len(), 0);
    block_on(run.into_future()).unwrap();
    assert_eq!(transport.requests().len(), 1);
}

#[test]
fn the_handle_is_send_and_static_and_can_run_on_another_thread() {
    let client = client(run_of("t1", "r1", text("m1", "x")));
    let run = client.run(request());
    let report = std::thread::spawn(move || block_on(run.into_future()))
        .join()
        .unwrap()
        .unwrap();
    assert!(report.status.is_success());
}

#[test]
fn cancelling_wakes_a_parked_run_and_never_synthesizes_a_finish() {
    let transport = StallingTransport {
        frames: vec![run_started("t1", "r1")],
        ..Default::default()
    };
    let client = AgentClient::with_transport(transport.clone())
        .build()
        .unwrap();
    let token = CancelToken::new();
    let run = client.run(request().cancel_token(token.clone()));
    let canceller = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(50));
        token.cancel();
    });
    let err = block_on(run.into_future()).unwrap_err();
    canceller.join().unwrap();
    assert_eq!(err.code(), ErrorCode::Cancelled);
    let partial = err.partial().unwrap();
    assert!(!partial.status.is_success());
    assert_eq!(transport.opens.load(Ordering::SeqCst), 1);
    // The transport saw the same token, so it could close at once.
    assert!(transport
        .seen_cancel
        .lock()
        .unwrap()
        .as_ref()
        .unwrap()
        .is_cancelled());
}

#[test]
fn handle_cancel_ends_the_run_with_run_cancelled() {
    let transport = StallingTransport::default();
    let client = AgentClient::with_transport(transport).build().unwrap();
    let mut run = client.run(request());
    let token = run.cancel_token();
    assert!(!token.is_cancelled());
    run.cancel();
    assert!(token.is_cancelled());
    let err = block_on(run.next()).unwrap().unwrap_err();
    assert_eq!(err.code(), ErrorCode::Cancelled);
    assert!(err.partial().is_none(), "nothing was delivered");
}

#[derive(Default)]
struct Recorder {
    events: AtomicUsize,
    finishes: Mutex<Vec<(String, Result<bool, ErrorCode>)>>,
    diagnostics: Mutex<Vec<(DiagnosticCode, String)>>,
}

impl Observer for Recorder {
    fn on_event(&self, update: &RunUpdate, view: RunView<'_>) {
        self.events.fetch_add(1, Ordering::SeqCst);
        assert_eq!(view.run().run_id().as_str(), view.run_id().as_str());
        if update.event.event_type() == EventType::TextMessageContent {
            assert_eq!(view.messages().last().unwrap().text(), Some("Hi"));
        }
        let _ = (view.state(), view.pending_tool_calls(), view.thread_id());
    }

    fn on_diagnostic(&self, diagnostic: &Diagnostic, run: RunInfo<'_>) {
        self.diagnostics
            .lock()
            .unwrap()
            .push((diagnostic.code, run.run_id().as_str().to_owned()));
    }

    fn on_finish(&self, result: Result<&RunReport, &Error>, run: RunInfo<'_>) {
        let result = result.map(|r| r.status.is_success()).map_err(Error::code);
        self.finishes
            .lock()
            .unwrap()
            .push((run.thread_id().as_str().to_owned(), result));
    }
}

fn observed(frames: Vec<Value>, recorder: &Arc<Recorder>) -> AgentClient<MockTransport> {
    AgentClient::with_transport(mock(frames))
        .diagnostic_sink(NullSink)
        .observer(Shared(Arc::clone(recorder)))
        .build()
        .unwrap()
}

struct Shared(Arc<Recorder>);

impl Observer for Shared {
    fn on_event(&self, update: &RunUpdate, view: RunView<'_>) {
        self.0.on_event(update, view)
    }
    fn on_diagnostic(&self, diagnostic: &Diagnostic, run: RunInfo<'_>) {
        self.0.on_diagnostic(diagnostic, run)
    }
    fn on_finish(&self, result: Result<&RunReport, &Error>, run: RunInfo<'_>) {
        self.0.on_finish(result, run)
    }
}

#[test]
fn observers_see_every_event_and_finish_exactly_once() {
    let recorder = Arc::new(Recorder::default());
    let client = observed(run_of("t1", "r1", text("m1", "Hi")), &recorder);
    block_on(client.run(request()).into_future()).unwrap();
    assert_eq!(recorder.events.load(Ordering::SeqCst), 5);
    assert_eq!(
        *recorder.finishes.lock().unwrap(),
        vec![("t1".to_owned(), Ok(true))]
    );
}

#[test]
fn observers_are_told_which_run_a_diagnostic_belongs_to() {
    let recorder = Arc::new(Recorder::default());
    let frames = vec![
        run_started("t1", "r1"),
        json!({"type": "SOMETHING_NEW"}),
        run_finished("t1", "r1"),
    ];
    let client = observed(frames, &recorder);
    let report = block_on(client.run(request()).into_future()).unwrap();
    let seen = recorder.diagnostics.lock().unwrap().clone();
    assert_eq!(seen, vec![(DiagnosticCode::UnknownEvent, "r1".to_owned())]);
    assert_eq!(report.diagnostics.len(), 1);
    assert_eq!(
        report.diagnostics[0].run_id.as_ref().unwrap().as_str(),
        "r1"
    );
}

#[test]
fn dropping_the_handle_mid_run_still_finishes_observers_once() {
    let recorder = Arc::new(Recorder::default());
    let client = observed(run_of("t1", "r1", text("m1", "Hi")), &recorder);
    let mut run = client.run(request());
    block_on(run.next()).unwrap().unwrap();
    drop(run);
    assert_eq!(
        *recorder.finishes.lock().unwrap(),
        vec![("t1".to_owned(), Err(ErrorCode::Cancelled))]
    );
    // A finished run is not finished again by its drop.
    let recorder = Arc::new(Recorder::default());
    let client = observed(run_of("t1", "r1", vec![]), &recorder);
    let mut run = client.run(request());
    while block_on(run.next()).is_some() {}
    drop(run);
    assert_eq!(recorder.finishes.lock().unwrap().len(), 1);
}

#[test]
fn dropping_an_unpolled_handle_cancels_its_own_token() {
    let client = client(vec![]);
    let run = client.run(request());
    let token = run.cancel_token();
    drop(run);
    assert!(token.is_cancelled());
    // A caller's token is left alone.
    let token = CancelToken::new();
    drop(client.run(request().cancel_token(token.clone())));
    assert!(!token.is_cancelled());
}

struct Panicky;

impl Observer for Panicky {
    fn on_event(&self, update: &RunUpdate, _view: RunView<'_>) {
        if update.event.event_type() == EventType::TextMessageStart {
            panic!("observer bug");
        }
    }
}

#[test]
fn a_panicking_observer_fails_the_run_and_others_still_finish() {
    let recorder = Arc::new(Recorder::default());
    let client = AgentClient::with_transport(mock(run_of("t1", "r1", text("m1", "Hi"))))
        .diagnostic_sink(NullSink)
        .observer(Panicky)
        .observer(Shared(Arc::clone(&recorder)))
        .build()
        .unwrap();
    let err = block_on(client.run(request()).into_future()).unwrap_err();
    assert_eq!(err.code(), ErrorCode::ObserverPanicked);
    assert!(err.partial().is_some());
    assert_eq!(
        *recorder.finishes.lock().unwrap(),
        vec![("t1".to_owned(), Err(ErrorCode::ObserverPanicked))]
    );
}

struct PanicsOnFinish;

impl Observer for PanicsOnFinish {
    fn on_finish(&self, _result: Result<&RunReport, &Error>, _run: RunInfo<'_>) {
        panic!("finish bug");
    }
}

#[test]
fn a_panic_in_on_finish_is_surfaced_on_an_otherwise_successful_run() {
    let client = AgentClient::with_transport(mock(run_of("t1", "r1", vec![])))
        .observer(PanicsOnFinish)
        .build()
        .unwrap();
    let err = block_on(client.run(request()).into_future()).unwrap_err();
    assert_eq!(err.code(), ErrorCode::ObserverPanicked);
    assert!(err.partial().unwrap().status.is_success());
}

#[test]
fn middleware_sees_raw_frames_before_enforcement_and_can_rewrite_them() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let log = Arc::clone(&seen);
    let frames = run_of(
        "t1",
        "r1",
        vec![json!({"type": "CUSTOM", "name": "n", "value": 1, "vendorField": true})],
    );
    let client = AgentClient::with_transport(mock(frames))
        .diagnostic_sink(NullSink)
        .middleware(move |request: MiddlewareRequest, next: Next| {
            let log = Arc::clone(&log);
            map_frames(next.run(request), move |mut frame: Frame| {
                let raw = frame.raw_type().map(|t| t.into_owned());
                let value = frame.value_mut()?;
                if raw.as_deref() == Some("CUSTOM") {
                    // Unknown material is still visible here (PROC-29).
                    log.lock().unwrap().push(value["vendorField"].clone());
                    value["value"] = json!(2);
                }
                Ok(Some(frame))
            })
        })
        .build()
        .unwrap();
    let report = block_on(client.run(request()).into_future()).unwrap();
    assert_eq!(*seen.lock().unwrap(), vec![json!(true)]);
    assert!(report.status.is_success());
    let mut run = client.run(request());
    let custom = block_on(async {
        loop {
            let update = run.next().await.unwrap().unwrap();
            if let Event::Custom(c) = &*update.event {
                break c.value.clone();
            }
        }
    });
    assert_eq!(custom, json!(2));
}

#[test]
fn middleware_order_is_first_registered_outermost_and_input_is_rewritable() {
    let order = Arc::new(Mutex::new(Vec::new()));
    let (a, b) = (Arc::clone(&order), Arc::clone(&order));
    let transport = mock(run_of("t1", "r1", vec![]));
    let client = AgentClient::with_transport(transport.clone())
        .middleware(move |mut request: MiddlewareRequest, next: Next| {
            a.lock().unwrap().push("outer");
            request.input["context"] = json!([{"description": "locale", "value": "en-GB"}]);
            next.run(request)
        })
        .middleware(move |request: MiddlewareRequest, next: Next| {
            b.lock().unwrap().push("inner");
            next.run(request)
        })
        .build()
        .unwrap();
    block_on(client.run(request()).into_future()).unwrap();
    assert_eq!(*order.lock().unwrap(), ["outer", "inner"]);
    let sent = transport.last_request().unwrap();
    assert_eq!(sent["context"][0]["value"], "en-GB");
}

#[test]
fn middleware_can_drop_and_insert_frames() {
    let frames = run_of("t1", "r1", text("m1", "drop me"));
    let client = AgentClient::with_transport(mock(frames))
        .middleware(|request: MiddlewareRequest, next: Next| {
            map_frames(next.run(request), |frame: Frame| {
                Ok(match frame.event_type() {
                    Some(
                        EventType::TextMessageStart
                        | EventType::TextMessageContent
                        | EventType::TextMessageEnd,
                    ) => None,
                    _ => Some(frame),
                })
            })
        })
        .build()
        .unwrap();
    let report = block_on(client.run(request()).into_future()).unwrap();
    assert_eq!(report.messages.len(), 1, "only the sent user message");
}

#[test]
fn a_panicking_middleware_fails_the_run() {
    let client = AgentClient::with_transport(mock(run_of("t1", "r1", vec![])))
        .middleware(|_request: MiddlewareRequest, _next: Next| -> FrameStream { panic!("mw bug") })
        .build()
        .unwrap();
    let err = block_on(client.run(request()).into_future()).unwrap_err();
    assert_eq!(err.code(), ErrorCode::MiddlewareFailed);

    let client = AgentClient::with_transport(mock(run_of("t1", "r1", vec![])))
        .middleware(|request: MiddlewareRequest, next: Next| {
            map_frames(
                next.run(request),
                |_frame| -> agui_sdk::Result<Option<Frame>> { panic!("per-frame bug") },
            )
        })
        .build()
        .unwrap();
    let err = block_on(client.run(request()).into_future()).unwrap_err();
    assert_eq!(err.code(), ErrorCode::MiddlewareFailed);
}

#[test]
fn the_frame_guard_refuses_oversized_frames_on_both_paths() {
    let mut limits = Limits::default();
    limits.max_frame_bytes = 64;
    let frames = run_of("t1", "r1", text("m1", &"x".repeat(100)));
    let client = AgentClient::with_transport(mock(frames.clone()))
        .limits(limits)
        .build()
        .unwrap();
    let err = block_on(client.run(request()).into_future()).unwrap_err();
    assert_eq!(err.code(), ErrorCode::FrameTooLarge);

    let client = AgentClient::with_transport(mock(frames))
        .limits(limits)
        .middleware(|request: MiddlewareRequest, next: Next| next.run(request))
        .build()
        .unwrap();
    let err = block_on(client.run(request()).into_future()).unwrap_err();
    assert_eq!(err.code(), ErrorCode::FrameTooLarge);
}

#[test]
fn a_zero_budget_is_a_config_error() {
    let mut limits = Limits::default();
    limits.max_depth = 0;
    let err = AgentClient::with_transport(mock(vec![]))
        .limits(limits)
        .build()
        .unwrap_err();
    assert_eq!(err.code(), ErrorCode::InvalidConfig);
}

#[test]
fn parsed_frames_from_a_transport_go_through_the_boundary() {
    // A retired THINKING_* shape arriving as a parsed value is translated,
    // as it would be on the byte path.
    let frames = vec![
        run_started("t1", "r1"),
        json!({"type": "THINKING_START"}),
        json!({"type": "THINKING_END"}),
        run_finished("t1", "r1"),
    ];
    let sink = Arc::new(CollectingSink::new());
    let client = AgentClient::with_transport(ValueTransport(frames))
        .diagnostic_sink(Arc::clone(&sink))
        .build()
        .unwrap();
    let report = block_on(client.run(request()).into_future()).unwrap();
    assert!(report.status.is_success());
    assert!(report
        .diagnostics
        .iter()
        .any(|d| d.code == DiagnosticCode::RetiredShape));
}

/// Counts the events a run delivered.
struct EventCount(Arc<AtomicUsize>);

impl Observer for EventCount {
    fn on_event(&self, _: &RunUpdate, _: RunView<'_>) {
        self.0.fetch_add(1, Ordering::Relaxed);
    }
}

/// What the compatibility boundary keeps for retired `THINKING_*` spans is
/// charged to the history budget on the middleware path too, as on the
/// direct path (D28): a flood of open spans with distinct ids reaches the
/// limit no later than without middleware. Before, the middleware path's
/// boundary was never charged, so it held an uncharged copy of every id.
#[test]
fn the_middleware_paths_boundary_is_charged_like_the_direct_paths() {
    let mut limits = Limits::default();
    limits.max_history_bytes = 1024 * 1024;
    let mut frames = vec![run_started("t1", "r1")];
    frames.extend((0..2048).map(
        |i| json!({"type": "THINKING_START", "messageId": format!("{i:06}{}", "x".repeat(4090))}),
    ));
    frames.push(run_finished("t1", "r1"));
    let delivered = |middleware: bool| {
        let count = Arc::new(AtomicUsize::new(0));
        let mut builder = AgentClient::with_transport(mock(frames.clone()))
            .limits(limits)
            .diagnostic_sink(NullSink)
            .observer(EventCount(Arc::clone(&count)));
        if middleware {
            builder =
                builder.middleware(|request: MiddlewareRequest, next: Next| next.run(request));
        }
        let err = block_on(builder.build().unwrap().run(request()).into_future()).unwrap_err();
        assert_eq!(
            err.code(),
            ErrorCode::LimitExceeded,
            "middleware: {middleware}"
        );
        count.load(Ordering::Relaxed)
    };
    let direct = delivered(false);
    let via_middleware = delivered(true);
    assert!(
        via_middleware <= direct + 2,
        "{via_middleware} events delivered through middleware, {direct} without"
    );
}

#[test]
fn reserved_or_malformed_per_run_headers_fail_before_sending() {
    for (name, value) in [("Content-Type", "x"), ("bad name", "x"), ("X-Ok", "a\r\nb")] {
        let transport = mock(run_of("t1", "r1", vec![]));
        let client = AgentClient::with_transport(transport.clone())
            .build()
            .unwrap();
        let err = block_on(client.run(request().header(name, value)).into_future()).unwrap_err();
        assert_eq!(err.code(), ErrorCode::InvalidRequest, "{name}");
        assert_eq!(transport.requests().len(), 0);
    }
    let transport = mock(run_of("t1", "r1", vec![]));
    let client = AgentClient::with_transport(transport.clone())
        .build()
        .unwrap();
    block_on(
        client
            .run(
                request()
                    .header("X-Tenant", "a")
                    .sensitive_header("X-Key", "s"),
            )
            .into_future(),
    )
    .unwrap();
    assert_eq!(transport.requests().len(), 1);
}

#[test]
fn subscribers_can_stop_propagation_and_replace_reported_state() {
    let calls = Arc::new(AtomicUsize::new(0));
    let (c1, c2) = (Arc::clone(&calls), Arc::clone(&calls));
    let subscriber = Subscriber::new()
        .on(EventType::RunFinished, move |_event, mutation| {
            c1.fetch_add(1, Ordering::SeqCst);
            mutation.set_state(json!({"replaced": true}));
            mutation.set_messages(vec![Message::user("only")]);
            Flow::Stop
        })
        .on_any(move |_event, _mutation| {
            c2.fetch_add(100, Ordering::SeqCst);
            Flow::Continue
        });
    let client = AgentClient::with_transport(mock(run_of("t1", "r1", vec![])))
        .subscriber(subscriber)
        .build()
        .unwrap();
    let report = block_on(client.run(request()).into_future()).unwrap();
    // on_any saw RUN_STARTED only; RUN_FINISHED stopped at the first callback.
    assert_eq!(calls.load(Ordering::SeqCst), 101);
    assert_eq!(report.state, json!({"replaced": true}));
    assert_eq!(report.messages.len(), 1);
    assert_eq!(report.messages[0].text(), Some("only"));
}

#[test]
fn a_panicking_subscriber_fails_the_run() {
    let subscriber = Subscriber::new().on(EventType::RunStarted, |_e, _m| panic!("sub bug"));
    let client = AgentClient::with_transport(mock(run_of("t1", "r1", vec![])))
        .subscriber(subscriber)
        .build()
        .unwrap();
    let err = block_on(client.run(request()).into_future()).unwrap_err();
    assert_eq!(err.code(), ErrorCode::ObserverPanicked);
}

#[test]
fn updates_share_the_event_with_the_reducer() {
    let frames = run_of(
        "t1",
        "r1",
        vec![json!({"type": "STATE_SNAPSHOT", "snapshot": {"big": [1, 2, 3]}})],
    );
    let client = client(frames);
    let mut run = client.run(request());
    // While the snapshot is the current state, the handle reads it through
    // the very event the update carries: shared, not deep-copied.
    let shared = block_on(async {
        let mut shared = None;
        while let Some(update) = run.next().await {
            let update = update.unwrap();
            if let Event::StateSnapshot(installed) = &*update.event {
                assert!(update.changes.state);
                shared = Some(std::ptr::eq(run.state(), &installed.snapshot));
            }
        }
        shared
    });
    assert_eq!(shared, Some(true), "state is read through the shared event");
    assert_eq!(run.state(), &json!({"big": [1, 2, 3]}));
}

#[test]
fn a_multi_run_stream_accumulates_and_reports_the_last_run() {
    let mut frames = run_of("t1", "r0", text("m0", "first"));
    frames.extend(run_of("t1", "r1", text("m1", "second")));
    let client = client(frames);
    let report = block_on(client.run(request()).into_future()).unwrap();
    assert_eq!(report.run_id.as_str(), "r1");
    assert_eq!(report.messages.len(), 3);
}

#[test]
fn the_requested_run_need_not_be_last_but_must_appear() {
    // The agent restarts after the requested run failed: accepted, and the
    // report describes the last run on the stream.
    let mut frames = vec![
        run_started("t1", "r1"),
        json!({"type": "RUN_ERROR", "message": "retrying"}),
    ];
    frames.extend(run_of("t1", "r2", text("m2", "second")));
    let report = block_on(client(frames).run(request()).into_future()).unwrap();
    assert_eq!(report.run_id.as_str(), "r2");
    assert_eq!(report.messages.len(), 2);

    // Every run names another id: the request was never answered.
    let mut frames = run_of("t1", "r0", text("m0", "replayed"));
    frames.extend(run_of("t1", "r2", vec![]));
    let err = block_on(client(frames).run(request()).into_future()).unwrap_err();
    assert_eq!(err.code(), ErrorCode::Identity);
    assert!(err.message().contains("'r1'"), "{}", err.message());
    assert_eq!(err.partial().unwrap().messages.len(), 2);

    // Another thread on a later run is refused at its RUN_STARTED.
    let mut frames = run_of("t1", "r1", vec![]);
    frames.extend(run_of("t2", "r2", vec![]));
    let err = block_on(client(frames).run(request()).into_future()).unwrap_err();
    assert_eq!(err.code(), ErrorCode::Identity);
}

#[test]
fn middleware_sees_frames_after_the_compatibility_boundary() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let log = Arc::clone(&seen);
    let frames = vec![
        run_started("t1", "r1"),
        json!({"type": "THINKING_START"}),
        json!({"type": "THINKING_END"}),
        run_finished("t1", "r1"),
    ];
    let client = AgentClient::with_transport(mock(frames))
        .diagnostic_sink(NullSink)
        .middleware(move |request: MiddlewareRequest, next: Next| {
            let log = Arc::clone(&log);
            map_frames(next.run(request), move |frame: Frame| {
                log.lock()
                    .unwrap()
                    .push(frame.raw_type().unwrap().into_owned());
                Ok(Some(frame))
            })
        })
        .build()
        .unwrap();
    block_on(client.run(request()).into_future()).unwrap();
    let seen = seen.lock().unwrap();
    assert!(seen.iter().all(|t| !t.starts_with("THINKING")), "{seen:?}");
    assert!(seen.iter().any(|t| t.starts_with("REASONING")), "{seen:?}");
}

struct PanicsOnDiagnostic;

impl Observer for PanicsOnDiagnostic {
    fn on_diagnostic(&self, _diagnostic: &Diagnostic, _run: RunInfo<'_>) {
        panic!("diagnostic bug");
    }
}

#[test]
fn a_panic_in_on_diagnostic_fails_the_run() {
    let frames = run_of("t1", "r1", vec![json!({"type": "VENDOR_EVENT"})]);
    let client = AgentClient::with_transport(mock(frames))
        .diagnostic_sink(NullSink)
        .observer(PanicsOnDiagnostic)
        .build()
        .unwrap();
    let err = block_on(client.run(request()).into_future()).unwrap_err();
    assert_eq!(err.code(), ErrorCode::ObserverPanicked);
}

#[test]
fn a_long_ready_stream_yields_to_the_executor() {
    use std::future::Future;
    use std::task::{Context, Poll, Wake, Waker};
    struct Count(AtomicUsize);
    impl Wake for Count {
        fn wake(self: Arc<Self>) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }
    let mut body = Vec::new();
    for i in 0..600 {
        body.push(json!({"type": "CUSTOM", "name": format!("n{i}"), "value": i}));
    }
    let client = client(run_of("t1", "r1", body));
    let mut future = Box::pin(client.run(request()).into_future());
    let count = Arc::new(Count(AtomicUsize::new(0)));
    let waker = Waker::from(Arc::clone(&count));
    let mut cx = Context::from_waker(&waker);
    let mut polls = 0;
    let report = loop {
        polls += 1;
        match future.as_mut().poll(&mut cx) {
            Poll::Ready(report) => break report.unwrap(),
            Poll::Pending => assert!(count.0.load(Ordering::SeqCst) >= polls - 1),
        }
    };
    assert!(report.status.is_success());
    assert!(polls > 2, "602 ready frames took {polls} polls");
    assert_eq!(
        count.0.load(Ordering::SeqCst),
        polls - 1,
        "every yield woke the task"
    );
}

/// Polls `run` to completion, counting polls and wakes.
fn polls_and_wakes(client: &AgentClient<MockTransport>) -> (RunReport, usize, usize) {
    use std::future::Future;
    use std::task::{Context, Poll, Wake, Waker};
    struct Count(AtomicUsize);
    impl Wake for Count {
        fn wake(self: Arc<Self>) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }
    let mut future = Box::pin(client.run(request()).into_future());
    let count = Arc::new(Count(AtomicUsize::new(0)));
    let waker = Waker::from(Arc::clone(&count));
    let mut cx = Context::from_waker(&waker);
    let mut polls = 0;
    let report = loop {
        polls += 1;
        match future.as_mut().poll(&mut cx) {
            Poll::Ready(report) => break report.unwrap(),
            Poll::Pending => assert!(polls < 10_000, "no progress"),
        }
    };
    (report, polls, count.0.load(Ordering::SeqCst))
}

#[test]
fn a_map_frames_that_drops_a_long_run_of_frames_yields_to_the_executor() {
    let mut body = Vec::new();
    for i in 0..600 {
        body.push(json!({"type": "CUSTOM", "name": format!("n{i}"), "value": i}));
    }
    let client = AgentClient::with_transport(mock(run_of("t1", "r1", body)))
        .diagnostic_sink(NullSink)
        .middleware(|request: MiddlewareRequest, next: Next| {
            map_frames(next.run(request), |frame: Frame| {
                Ok(match frame.event_type() {
                    Some(EventType::Custom) => None,
                    _ => Some(frame),
                })
            })
        })
        .build()
        .unwrap();
    let (report, polls, wakes) = polls_and_wakes(&client);
    assert!(report.status.is_success());
    assert!(polls > 2, "600 dropped frames took {polls} polls");
    assert_eq!(wakes, polls - 1, "every yield woke the task");
}

#[test]
fn an_era_shim_that_drops_a_long_run_of_frames_yields_to_the_executor() {
    let mut body = Vec::new();
    for i in 0..600 {
        body.push(
            json!({"type": "SUBAGENT_STARTED", "subagentRunId": format!("s{i}"), "name": "h"}),
        );
    }
    let client = AgentClient::with_transport(mock(run_of("t1", "r1", body)))
        .diagnostic_sink(NullSink)
        .peer_version(agui_sdk::version::PeerPin::Legacy(
            agui_sdk::version::LegacyEra::V0_0_57,
        ))
        .middleware(|request: MiddlewareRequest, next: Next| next.run(request))
        .build()
        .unwrap();
    let (report, polls, wakes) = polls_and_wakes(&client);
    assert!(report.status.is_success());
    assert!(polls > 2, "600 dropped frames took {polls} polls");
    assert_eq!(wakes, polls - 1, "every yield woke the task");
}

#[test]
fn the_report_keeps_a_bounded_number_of_diagnostics_but_the_sink_sees_all() {
    let sink = Arc::new(CollectingSink::new());
    let body = vec![json!({"type": "VENDOR_EVENT"}); 5000];
    let client = AgentClient::with_transport(mock(run_of("t1", "r1", body)))
        .diagnostic_sink(Arc::clone(&sink))
        .build()
        .unwrap();
    let report = block_on(client.run(request()).into_future()).unwrap();
    assert!(
        report.diagnostics.len() <= 4097,
        "the report kept {}",
        report.diagnostics.len()
    );
    assert_eq!(
        report.diagnostics.last().unwrap().code,
        DiagnosticCode::Limit
    );
    assert!(sink.snapshot().len() >= 5000, "forwarding is unchanged");
}

#[test]
fn the_report_diagnostic_cap_is_configurable_and_zero_keeps_only_the_summary() {
    for (cap, kept) in [(10usize, 10usize), (0, 0)] {
        let mut limits = Limits::default();
        limits.max_report_diagnostics = cap;
        let body = vec![json!({"type": "VENDOR_EVENT"}); 50];
        let client = AgentClient::with_transport(mock(run_of("t1", "r1", body)))
            .diagnostic_sink(NullSink)
            .limits(limits)
            .build()
            .unwrap();
        let report = block_on(client.run(request()).into_future()).unwrap();
        assert_eq!(report.diagnostics.len(), kept + 1);
        let summary = report.diagnostics.last().unwrap();
        assert_eq!(summary.code, DiagnosticCode::Limit);
        assert_eq!(summary.run_id.as_ref().unwrap().as_str(), "r1");
    }
}

#[test]
fn a_panic_in_the_diagnostic_sink_fails_the_run_and_says_so() {
    let frames = run_of("t1", "r1", vec![json!({"type": "VENDOR_EVENT"})]);
    let client = AgentClient::with_transport(mock(frames))
        .diagnostic_sink(|_: &Diagnostic| panic!("sink"))
        .build()
        .unwrap();
    let err = block_on(client.run(request()).into_future()).unwrap_err();
    assert_eq!(err.code(), ErrorCode::ObserverPanicked);
    assert!(err.to_string().contains("diagnostic sink"), "{err}");
}

struct CancelOnStart(CancelToken);

impl Observer for CancelOnStart {
    fn on_event(&self, update: &RunUpdate, _view: RunView<'_>) {
        if update.event.event_type() == EventType::RunStarted {
            self.0.cancel();
        }
    }
}

#[test]
fn cancelling_stops_a_stream_that_never_waits() {
    // Every frame is ready at once, so the run never parks: the token is
    // still honoured between events.
    let token = CancelToken::new();
    let client = AgentClient::with_transport(mock(run_of("t1", "r1", text("m1", "Hi"))))
        .observer(CancelOnStart(token.clone()))
        .build()
        .unwrap();
    let err = block_on(client.run(request().cancel_token(token)).into_future()).unwrap_err();
    assert_eq!(err.code(), ErrorCode::Cancelled);
    assert!(err.partial().unwrap().messages.len() < 2);
}

#[test]
fn a_stopping_subscriber_hides_the_event_from_later_subscribers() {
    let later = Arc::new(AtomicUsize::new(0));
    let seen = Arc::clone(&later);
    let client = AgentClient::with_transport(mock(run_of("t1", "r1", vec![])))
        .subscriber(Subscriber::new().on(EventType::RunStarted, |_e, _m| Flow::Stop))
        .subscriber(Subscriber::new().on_any(move |_e, _m| {
            seen.fetch_add(1, Ordering::SeqCst);
            Flow::Continue
        }))
        .build()
        .unwrap();
    block_on(client.run(request()).into_future()).unwrap();
    // Only RUN_FINISHED reached the second subscriber.
    assert_eq!(later.load(Ordering::SeqCst), 1);
}

#[test]
fn era_shims_drop_frames_on_the_parsed_frame_path() {
    // A pre-0.0.58 peer has no subagents: the shim drops the event, so it
    // never reaches the pipeline (which would reject the unknown invocation).
    let frames = run_of(
        "t1",
        "r1",
        vec![json!({"type": "SUBAGENT_STARTED", "subagentRunId": "s1", "name": "helper"})],
    );
    let client = AgentClient::with_transport(ValueTransport(frames))
        .diagnostic_sink(NullSink)
        .peer_version(agui_sdk::version::PeerPin::Legacy(
            agui_sdk::version::LegacyEra::V0_0_57,
        ))
        .build()
        .unwrap();
    let mut run = client.run(request());
    let types = block_on(async {
        let mut types = Vec::new();
        while let Some(update) = run.next().await {
            types.push(update.unwrap().event.event_type());
        }
        types
    });
    assert_eq!(types, [EventType::RunStarted, EventType::RunFinished]);
}

#[test]
fn a_thread_absorbs_the_partial_history_of_a_failed_run() {
    let mut frames = vec![run_started("t1", "r1")];
    frames.extend(text("m1", "half an answer"));
    let client = AgentClient::with_transport(mock(frames)).build().unwrap();
    let mut thread = client.thread_with_id("t1");
    let err =
        block_on(thread.run_and_absorb(RunRequest::new().run_id("r1").user("q"))).unwrap_err();
    assert_eq!(err.code(), ErrorCode::Truncated);
    assert_eq!(thread.messages().len(), 2);
    assert_eq!(thread.messages()[1].text(), Some("half an answer"));
    assert_eq!(thread.last_run_id().unwrap().as_str(), "r1");
}

#[test]
fn null_state_and_props_read_as_absent_in_the_input() {
    let input = request()
        .state_value(Value::Null)
        .forwarded_props_value(Value::Null)
        .into_input()
        .unwrap();
    assert_eq!(input.state, None);
    assert_eq!(input.forwarded_props, None);
}

/// A producer MAY start a new run after `RUN_ERROR` on the same stream. The
/// report keeps the requested run's failure beside the last run's success,
/// so the failed run is never reported as having succeeded, and a thread
/// absorbs the outcome and runs again instead of being left wedged.
///
/// covers: LIFE-23, LIFE-24, LIFE-45
#[test]
fn a_run_restarted_after_run_error_reports_both_and_never_wedges_a_thread() {
    use agui_sdk::client::Thread;

    let mut restarted = vec![
        run_started("t1", "r1"),
        json!({"type": "RUN_ERROR", "message": "first run failed", "code": "boom"}),
    ];
    restarted.extend(run_of("t1", "r2", text("m1", "second run")));

    let report = block_on(client(restarted.clone()).run(request()).into_future()).unwrap();
    assert_eq!(report.requested_run_id.as_str(), "r1");
    match &report.requested_status {
        RunStatus::Errored(error) => assert_eq!(error.message, "first run failed"),
        other => panic!("the requested run failed, not {other:?}"),
    }
    assert!(!report.requested_status.is_success());
    // The stream's last run, and what it added (LIFE-45).
    assert_eq!(report.run_id.as_str(), "r2");
    assert!(report.status.is_success());
    assert_eq!(report.new_message_ids.len(), 1);

    // A plain run keeps both views equal.
    let plain = block_on(
        client(run_of("t1", "r1", vec![]))
            .run(request())
            .into_future(),
    )
    .unwrap();
    assert_eq!(plain.requested_run_id, plain.run_id);
    assert_eq!(plain.requested_status, plain.status);

    // On a thread: absorbed, and the next run starts.
    let transport = RunScripts::json(vec![restarted, run_of("t1", "r3", text("m2", "again"))]);
    let client = AgentClient::with_transport(transport)
        .diagnostic_sink(NullSink)
        .build()
        .unwrap();
    let mut thread: Thread<RunScripts> = client.thread_with_id("t1");
    let report =
        block_on(thread.run_and_absorb(RunRequest::new().run_id("r1").user("hi"))).unwrap();
    assert_eq!(report.run_id.as_str(), "r2");
    assert_eq!(thread.last_run_id().map(|id| id.as_str()), Some("r2"));
    assert_eq!(thread.messages().len(), 2);
    block_on(thread.run_and_absorb(RunRequest::new().run_id("r3").user("more"))).unwrap();
    assert_eq!(thread.messages().len(), 4);
}

/// An outcome that is not the in-flight run's is refused, and the thread is
/// left able to run again rather than refusing every later run.
#[test]
fn absorbing_the_wrong_outcome_does_not_wedge_the_thread() {
    use agui_sdk::client::Thread;

    let transport = RunScripts::json(vec![run_of("t1", "r1", vec![]), run_of("t1", "r2", vec![])]);
    let client = AgentClient::with_transport(transport)
        .diagnostic_sink(NullSink)
        .build()
        .unwrap();
    let mut thread: Thread<RunScripts> = client.thread_with_id("t1");
    let _pending = thread.run(RunRequest::new().run_id("r1")).unwrap();
    let elsewhere = AgentClient::with_transport(mock(run_of("t9", "r9", vec![])))
        .diagnostic_sink(NullSink)
        .build()
        .unwrap();
    let other = block_on(
        elsewhere
            .run(RunRequest::new().thread_id("t9").run_id("r9"))
            .into_future(),
    );
    let err = thread.absorb(&other).expect_err("not the run in flight");
    assert_eq!(err.code(), ErrorCode::InvalidRequest);
    assert!(thread.run(RunRequest::new().run_id("r2")).is_ok());
}

#[test]
fn new_messages_follow_the_new_ids_and_take_the_first_message_with_an_id() {
    let mut report = RunReport::new(
        "t1".into(),
        "r1".into(),
        RunStatus::Finished {
            outcome: None,
            result: None,
        },
    );
    report.messages = vec![
        Message::user("old"),
        Message::assistant("first"),
        Message::assistant("second"),
    ];
    let ids: Vec<_> = report.messages.iter().map(|m| m.id().clone()).collect();
    // Reordered relative to the history; `old` is not new.
    report.new_message_ids = vec![ids[2].clone(), ids[1].clone()];
    let texts: Vec<_> = report.new_messages().map(|m| m.text().unwrap()).collect();
    assert_eq!(texts, ["second", "first"]);
}

#[test]
fn new_messages_take_the_first_of_two_messages_sharing_an_id() {
    let mut report = RunReport::new(
        "t1".into(),
        "r1".into(),
        RunStatus::Finished {
            outcome: None,
            result: None,
        },
    );
    let first = Message::assistant("first");
    let second = Message::Assistant(
        agui_sdk::message::AssistantMessage::new(first.id().clone()).with_content("second"),
    );
    report.new_message_ids = vec![first.id().clone()];
    report.messages = vec![first, second];
    let texts: Vec<_> = report.new_messages().map(|m| m.text().unwrap()).collect();
    assert_eq!(texts, ["first"]);
}

/// A tool result sharing its parentless call's id is the
/// second new message with that id, and `new_messages` yields both.
#[test]
fn new_messages_resolves_a_repeated_id_by_occurrence() {
    let mut body = vec![
        json!({"type": "TOOL_CALL_START", "toolCallId": "x", "toolCallName": "f"}),
        json!({"type": "TOOL_CALL_END", "toolCallId": "x"}),
        json!({"type": "TOOL_CALL_RESULT", "messageId": "x", "toolCallId": "x", "content": "ok"}),
    ];
    let frames = run_of("t1", "r1", std::mem::take(&mut body));
    let report = block_on(client(frames).run(request()).into_future()).unwrap();
    let roles: Vec<_> = report
        .new_messages()
        .map(|m| matches!(m, Message::Tool(_)))
        .collect();
    assert_eq!(roles, [false, true], "{:?}", report.new_message_ids);
}

/// A middleware that drops the translated reasoning frames cannot let the
/// compatibility boundary's span table grow unchecked: the middleware-path
/// boundary enforces the history limit itself (D28), so a flood of distinct
/// legacy `THINKING_START` ids ends the run at the limit.
#[test]
fn a_middleware_dropping_reasoning_frames_still_bounds_the_boundary() {
    let mut limits = Limits::default();
    limits.max_history_bytes = 1024 * 1024;
    let long = |i: usize| format!("{i:08}{}", "x".repeat(64 * 1024 - 8));
    let events: Vec<Value> = (0..64)
        .map(|i| json!({"type": "THINKING_START", "messageId": long(i)}))
        .collect();
    // No frame after the flood reaches the processor: the terminal event is
    // left out, so only the boundary's own check can end the run at the limit.
    let mut frames = run_of("t1", "r1", events);
    frames.pop();
    let client = AgentClient::with_transport(mock(frames))
        .limits(limits)
        .middleware(|request: MiddlewareRequest, next: Next| {
            map_frames(next.run(request), |frame: Frame| {
                Ok(match frame.event_type() {
                    Some(
                        EventType::ReasoningStart
                        | EventType::ReasoningMessageStart
                        | EventType::ReasoningMessageContent
                        | EventType::ReasoningMessageEnd
                        | EventType::ReasoningEnd,
                    ) => None,
                    _ => Some(frame),
                })
            })
        })
        .build()
        .unwrap();
    let err = block_on(client.run(request()).into_future()).unwrap_err();
    assert_eq!(err.code(), ErrorCode::LimitExceeded, "{err}");
}
