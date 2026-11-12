//! The run driver: outgoing input, transport, middleware chain, processor,
//! observers, and the end-of-run bookkeeping (report, `on_finish`).
//!
//! One driver per run, boxed once. Per event it does one processor push, one
//! `VecDeque` move per processed event, and one virtual call per observer;
//! the history is borrowed, never cloned.

use std::collections::VecDeque;
use std::future::Future;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock, PoisonError};
use std::task::{Context, Poll};

use serde_json::Value;

use crate::cancel::{CancelToken, Cancelled};
use crate::client::transport::RESERVED_HEADERS;
use crate::diagnostics::{Diagnostic, DiagnosticCode, DiagnosticSink, Reporter, Scope};
use crate::error::{Error, ErrorCode, Result};
use crate::event::Event;
use crate::ids::{MessageId, RunId, ThreadId};
use crate::lifecycle::{RunError, RunFinishedOutcome};
use crate::message::{Message, Role};
use crate::pipeline::compat::{Boundary, EraShims};
use crate::pipeline::enforce::Enforcer;
use crate::pipeline::{PipelineConfig, Processed, Processor, Termination};
use crate::run_input::RunAgentInput;
use crate::version::PeerPin;
use crate::wire::{Chunk, WireFormat};

use super::{RunReport, RunStatus, RunUpdate};
use crate::client::middleware::{
    Frame, FrameSource, FrameStream, Middleware, MiddlewareRequest, Next,
};
use crate::client::subscriber::{Flow, Mutation};
use crate::client::transport::input as outgoing;
use crate::client::transport::{Header, TransportRequest};
use crate::client::{ClientConfig, RunInfo, RunView};

/// The transport's `open`, type-erased and bound to its transport.
pub(in crate::client) type OpenFuture = Pin<Box<dyn Future<Output = Result<FrameStream>> + Send>>;
/// Opens the exchange for one request.
///
/// Every poll of an `OpenFuture`, and of the stream it yields, goes through
/// `catch_unwind` (`transport_panicked`): a custom transport is contained
/// like every other extension. A new poll site must do the same.
pub(in crate::client) type Open = Box<dyn FnOnce(TransportRequest) -> OpenFuture + Send>;

/// Frames processed in one `poll` before the driver yields to the executor.
pub(in crate::client) const FRAMES_PER_POLL: usize = 256;

/// Everything a run needs, assembled by the client or a thread.
pub(in crate::client) struct RunSetup {
    pub(in crate::client) config: Arc<ClientConfig>,
    pub(in crate::client) open: Open,
    pub(in crate::client) input: Result<RunAgentInput>,
    pub(in crate::client) headers: Vec<Header>,
    pub(in crate::client) cancel: Option<CancelToken>,
    pub(in crate::client) thread_id: ThreadId,
    pub(in crate::client) run_id: RunId,
}

enum Phase {
    /// Not polled yet: nothing sent.
    Idle(Box<Unsent>),
    /// Waiting for the transport to accept the request (no middleware).
    Opening(OpenFuture),
    /// Reading frames (from the transport, or the middleware chain).
    Streaming(FrameStream),
    /// Ended; `on_finish` delivered.
    Done,
}

/// A run not yet started.
struct Unsent {
    input: Result<RunAgentInput>,
    headers: Vec<Header>,
    open: Open,
}

/// The run's diagnostic sink: forwards to the client's sink and the
/// observers, tags the run id, and collects for the report.
struct RunSink {
    config: Arc<ClientConfig>,
    thread_id: ThreadId,
    run_id: RunId,
    /// What the report keeps, and how many diagnostics past the cap it did not.
    collected: Mutex<(Vec<Diagnostic>, usize)>,
    observer_panicked: AtomicBool,
}

impl DiagnosticSink for RunSink {
    fn report(&self, diagnostic: &Diagnostic) {
        let mut diagnostic = diagnostic.clone();
        if diagnostic.run_id.is_none() {
            diagnostic.run_id = Some(self.run_id.clone());
        }
        // The application's sink is user code: a panic fails the run like a
        // panicking observer, never unwinds through the transport's poll.
        let sink = &self.config.diagnostics;
        if catch_unwind(AssertUnwindSafe(|| sink.report(&diagnostic))).is_err() {
            self.observer_panicked.store(true, Ordering::Relaxed);
        }
        let run = RunInfo::new(&self.thread_id, &self.run_id);
        for observer in &self.config.observers {
            let call = catch_unwind(AssertUnwindSafe(|| {
                observer.on_diagnostic(&diagnostic, run)
            }));
            if call.is_err() {
                self.observer_panicked.store(true, Ordering::Relaxed);
            }
        }
        // Forwarding above is complete; only the report's copy is capped, so
        // a hostile stream cannot grow it without bound.
        let cap = self.config.limits.max_report_diagnostics;
        let mut collected = self
            .collected
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if collected.0.len() < cap {
            collected.0.push(diagnostic);
        } else {
            collected.1 = collected.1.saturating_add(1);
        }
    }
}

impl RunSink {
    /// The kept diagnostics, then one `diagnostics.limit` summary when some
    /// were not kept. The summary goes to the report only: the sink and the
    /// observers already saw every diagnostic.
    fn take(&self) -> Vec<Diagnostic> {
        let (mut kept, suppressed) = std::mem::take(
            &mut *self
                .collected
                .lock()
                .unwrap_or_else(PoisonError::into_inner),
        );
        if suppressed > 0 {
            let mut summary = Diagnostic::new(
                DiagnosticCode::Limit,
                Scope::Stream,
                "",
                format!(
                    "{suppressed} more diagnostics were not kept in the report (cap {}); \
                     each reached the diagnostic sink and observers",
                    self.config.limits.max_report_diagnostics
                ),
            );
            summary.run_id = Some(self.run_id.clone());
            kept.push(summary);
        }
        kept
    }
}

/// The run's state machine. `Send + 'static`.
pub(in crate::client) struct RunDriver {
    config: Arc<ClientConfig>,
    thread_id: ThreadId,
    run_id: RunId,
    cancel: CancelToken,
    owns_cancel: bool,
    cancelled: Option<Cancelled>,
    phase: Phase,
    via_middleware: bool,
    processor: Option<Processor>,
    boundary: Option<Boundary>,
    /// What the boundaries in the middleware path's `Upstream` stages keep
    /// (their `retained` figures, summed), charged like `boundary`'s (D28).
    upstream_retained: Arc<AtomicUsize>,
    era: Option<EraShims>,
    sink: Arc<RunSink>,
    reporter: Reporter,
    out: Vec<Processed>,
    ready: VecDeque<Processed>,
    started: bool,
    last_ids: Option<(ThreadId, RunId)>,
    /// The last run's terminal, and the requested run's.
    terminals: Terminals,
    /// A failure of a frame some of whose events were already applied: it
    /// concludes the run once those events are delivered.
    pending_error: Option<Error>,
    state_override: Option<Value>,
    messages_override: Option<Vec<Message>>,
    report: Option<RunReport>,
    finished: bool,
}

impl RunDriver {
    pub(in crate::client) fn new(setup: RunSetup) -> Self {
        let RunSetup {
            config,
            open,
            input,
            headers,
            cancel,
            thread_id,
            run_id,
        } = setup;
        let owns_cancel = cancel.is_none();
        let cancel = cancel.unwrap_or_default();
        let sink = Arc::new(RunSink {
            config: Arc::clone(&config),
            thread_id: thread_id.clone(),
            run_id: run_id.clone(),
            collected: Mutex::new((Vec::new(), 0)),
            observer_panicked: AtomicBool::new(false),
        });
        let reporter = Reporter::new(
            Arc::clone(&sink) as Arc<dyn DiagnosticSink>,
            config.limits.max_diagnostics_per_event,
        );
        RunDriver {
            config,
            thread_id,
            run_id,
            cancel,
            owns_cancel,
            cancelled: None,
            phase: Phase::Idle(Box::new(Unsent {
                input,
                headers,
                open,
            })),
            via_middleware: false,
            processor: None,
            boundary: None,
            upstream_retained: Arc::new(AtomicUsize::new(0)),
            era: None,
            sink,
            reporter,
            out: Vec::new(),
            ready: VecDeque::new(),
            started: false,
            last_ids: None,
            terminals: Terminals::default(),
            pending_error: None,
            state_override: None,
            messages_override: None,
            report: None,
            finished: false,
        }
    }

    pub(in crate::client) fn cancel_token(&self) -> &CancelToken {
        &self.cancel
    }

    pub(in crate::client) fn messages(&self) -> &[Message] {
        if let Some(messages) = &self.messages_override {
            return messages;
        }
        if let Some(processor) = &self.processor {
            return processor.reducer().messages();
        }
        match &self.report {
            Some(report) => &report.messages,
            None => &[],
        }
    }

    pub(in crate::client) fn message(&self, id: &MessageId) -> Option<&Message> {
        if self.messages_override.is_none() {
            if let Some(processor) = &self.processor {
                return processor.reducer().message(id);
            }
        }
        self.messages().iter().find(|m| m.id() == id)
    }

    pub(in crate::client) fn state(&self) -> &Value {
        if let Some(state) = &self.state_override {
            return state;
        }
        if let Some(processor) = &self.processor {
            return processor.reducer().state();
        }
        match &self.report {
            Some(report) => &report.state,
            None => empty_state(),
        }
    }

    /// The report of a run that ended successfully, once.
    pub(in crate::client) fn take_report(&mut self) -> Option<RunReport> {
        self.report.take()
    }

    pub(in crate::client) fn poll_next(
        &mut self,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<RunUpdate>>> {
        let mut frames = 0;
        loop {
            if self.finished {
                return Poll::Ready(None);
            }
            if self.cancel.is_cancelled() {
                if let Some(ended) = self.cancelled_now() {
                    return Poll::Ready(ended);
                }
            }
            if let Some(processed) = self.ready.pop_front() {
                return Poll::Ready(Some(self.deliver(processed)));
            }
            if let Some(err) = self.pending_error.take() {
                return Poll::Ready(Some(Err(self.conclude_err(err))));
            }
            if self.sink.observer_panicked.load(Ordering::Relaxed) {
                return Poll::Ready(Some(Err(self.conclude_err(observer_panicked()))));
            }
            match &mut self.phase {
                Phase::Idle(_) => {
                    let Phase::Idle(unsent) = std::mem::replace(&mut self.phase, Phase::Done)
                    else {
                        continue;
                    };
                    let Unsent {
                        input,
                        headers,
                        open,
                    } = *unsent;
                    if let Err(err) = self.start(input, headers, open) {
                        return Poll::Ready(Some(Err(self.conclude_err(err))));
                    }
                }
                Phase::Opening(future) => {
                    match catch_unwind(AssertUnwindSafe(|| future.as_mut().poll(cx)))
                        .unwrap_or_else(|_| Poll::Ready(Err(transport_panicked())))
                    {
                        Poll::Ready(Ok(stream)) => self.phase = Phase::Streaming(stream),
                        Poll::Ready(Err(err)) => {
                            return Poll::Ready(Some(Err(self.conclude_err(err))))
                        }
                        Poll::Pending => return self.park(cx),
                    }
                }
                Phase::Streaming(stream) => {
                    if frames == FRAMES_PER_POLL {
                        cx.waker().wake_by_ref();
                        return Poll::Pending;
                    }
                    // Through middleware, the transport is polled (and its
                    // panics contained) inside `Upstream`, so a panic caught
                    // here is the middleware's own.
                    let fallback = if self.via_middleware {
                        middleware_panicked
                    } else {
                        transport_panicked
                    };
                    let mut panicked = false;
                    let polled = catch_unwind(AssertUnwindSafe(|| stream.poll_next(cx)))
                        .unwrap_or_else(|_| {
                            panicked = true;
                            Poll::Ready(Some(Err(fallback())))
                        });
                    match polled {
                        Poll::Ready(Some(Ok(frame))) => {
                            frames += 1;
                            if let Err(err) = self.push(frame) {
                                if self.ready.is_empty() {
                                    return Poll::Ready(Some(Err(self.conclude_err(err))));
                                }
                                // Events of this frame were applied before
                                // it failed: deliver them, then fail.
                                self.pending_error = Some(err);
                            }
                        }
                        // After the requested run's terminal, a failing
                        // transport ends the stream as a dropped connection
                        // does: the terminal stands (transports/index.mdx).
                        Poll::Ready(Some(Err(err)))
                            if !panicked
                                && self.terminals.requested_concluded()
                                && ends_the_stream(err.code()) =>
                        {
                            return Poll::Ready(self.end_of_stream().map(Err));
                        }
                        // The transport saw the run's token first: the
                        // cancel is judged as one seen here would be, so a
                        // received terminal still wins.
                        Poll::Ready(Some(Err(err)))
                            if !panicked
                                && err.code() == ErrorCode::Cancelled
                                && self.cancel.is_cancelled() =>
                        {
                            if let Some(ended) = self.cancelled_now() {
                                return Poll::Ready(ended);
                            }
                        }
                        Poll::Ready(Some(Err(err))) => {
                            return Poll::Ready(Some(Err(self.conclude_err(err))))
                        }
                        Poll::Ready(None) => return Poll::Ready(self.end_of_stream().map(Err)),
                        Poll::Pending => return self.park(cx),
                    }
                }
                Phase::Done => return Poll::Ready(None),
            }
        }
    }

    /// Waits for the transport, waking early on cancellation.
    fn park(&mut self, cx: &mut Context<'_>) -> Poll<Option<Result<RunUpdate>>> {
        let cancel = &self.cancel;
        let cancelled = self.cancelled.get_or_insert_with(|| cancel.cancelled());
        match Pin::new(cancelled).poll(cx) {
            Poll::Ready(()) => match self.cancelled_now() {
                Some(ended) => Poll::Ready(ended),
                // A terminal was received: deliver what is ready first.
                None => {
                    cx.waker().wake_by_ref();
                    Poll::Pending
                }
            },
            Poll::Pending => Poll::Pending,
        }
    }

    /// The run's token is cancelled. Before the requested run's terminal
    /// event, the run ends with `run.cancelled` (an earlier run's terminal
    /// on a replayed stream does not count). Once it has been received, the
    /// server's verdict wins: the events already received are still
    /// delivered (`None` here) and the run concludes as that terminal says,
    /// without reading the transport any further. A failure already pending
    /// (`pending_error`) is the run's verdict and is surfaced instead.
    fn cancelled_now(&mut self) -> Option<Option<Result<RunUpdate>>> {
        if let Some(err) = self.pending_error.take() {
            return Some(Some(Err(self.conclude_err(err))));
        }
        if !self.terminals.requested_concluded() {
            return Some(Some(Err(self.conclude_err(cancelled_error()))));
        }
        if !self.ready.is_empty() {
            return None;
        }
        Some(self.end_of_stream().map(Err))
    }

    /// Prepares and sends the input (RUNIN-08, RUNIN-12, PROC-32..PROC-34).
    fn start(
        &mut self,
        input: Result<RunAgentInput>,
        headers: Vec<Header>,
        open: Open,
    ) -> Result<()> {
        let mut input = input?;
        check_headers(&headers)?;
        let config = Arc::clone(&self.config);
        let limits = config.limits;
        let legacy = match config.peer_version {
            PeerPin::Legacy(era) => Some(era),
            _ => None,
        };

        // The reducer keeps the whole history, activity messages included;
        // the peer gets the sendable part (RUNIN-12, ACT-18). Only a history
        // holding activity messages is copied; otherwise the input's
        // messages and state move to the reducer once the input is sent.
        let full_history = if input.messages.iter().any(|m| m.role() == Role::Activity) {
            let history = input.messages.clone();
            input.messages.retain(|m| m.role() != Role::Activity);
            Some(history)
        } else {
            None
        };
        input.protocol_version = match legacy {
            Some(_) => None,
            None => Some(crate::PROTOCOL_VERSION.to_owned()),
        };

        let request = RequestParts {
            thread_id: input.thread_id.clone(),
            run_id: input.run_id.clone(),
            headers,
            cancel: self.cancel.clone(),
            sink: Arc::clone(&self.sink) as Arc<dyn DiagnosticSink>,
            max_frame_bytes: limits.max_frame_bytes,
            max_container_entries: limits.max_container_entries,
        };
        let sent = self.send(&input, &config, legacy, request, open);

        // Built whether or not sending succeeded, so the handle's history
        // and state read the same either way.
        let history = full_history.unwrap_or_else(|| std::mem::take(&mut input.messages));
        let state = input.state.take().unwrap_or_else(empty_object);
        self.processor = Some(
            Processor::with_history(
                PipelineConfig {
                    limits,
                    legacy_peer: legacy,
                },
                history,
                state,
            )
            .expecting(input.thread_id, input.run_id),
        );
        sent
    }

    /// Encodes the input and opens the exchange, directly or through the
    /// middleware chain. The processor is not built yet: nothing is polled
    /// here.
    fn send(
        &mut self,
        input: &RunAgentInput,
        config: &ClientConfig,
        legacy: Option<crate::version::LegacyEra>,
        request: RequestParts,
        open: Open,
    ) -> Result<()> {
        let limits = config.limits;
        let enforcer = Enforcer::new(limits);
        let era = legacy.map(EraShims::new);
        if config.middleware.is_empty() {
            // Upgrade, era downgrade, enforcement, one serialization.
            let body = outgoing::encode_input(input, era.as_ref(), &enforcer, &mut self.reporter);
            self.reporter.end_event(None);
            let body = body?;
            self.boundary = Some(Boundary::new());
            self.era = era;
            self.phase = Phase::Opening(open(request.into_request(body)));
        } else {
            // The typed check first, so middleware never sees a malformed
            // input; then upgrade and era downgrade. Enforcement runs again
            // on what middleware hands on (`upstream`).
            enforcer.check_outgoing_input(input, &mut self.reporter)?;
            let value = outgoing::prepare_value(to_value(input)?, era.as_ref(), &mut self.reporter);
            self.reporter.end_event(None);
            let innermost = upstream(
                open,
                request,
                era,
                Arc::clone(&self.sink),
                limits,
                Arc::clone(&self.upstream_retained),
            );
            let request =
                MiddlewareRequest::new(value, Arc::clone(&self.sink) as Arc<dyn DiagnosticSink>);
            self.via_middleware = true;
            self.phase = Phase::Streaming(run_chain(&config.middleware, request, innermost));
        }
        Ok(())
    }

    /// One frame into the processor. Processed events queue in `ready`.
    fn push(&mut self, frame: Frame) -> Result<()> {
        guard_frame(&frame, self.config.limits.max_frame_bytes)?;
        let Some(processor) = self.processor.as_mut() else {
            return Err(Error::new(
                ErrorCode::Internal,
                "frame before the run started",
            ));
        };
        let result = if self.via_middleware {
            // What the boundary in `Upstream` keeps is charged with the
            // processor's own id bookkeeping (D28).
            processor.charge_boundary(self.upstream_retained.load(Ordering::Relaxed));
            frame
                .into_value()
                .and_then(|value| processor.push_value(value, &mut self.reporter, &mut self.out))
        } else if let Some(json) = frame.as_json() {
            processor.push_json(json, &mut self.reporter, &mut self.out)
        } else {
            // A transport that produced a parsed frame: the boundary and era
            // shims run here, as `push_json` would run them.
            frame.into_value().and_then(|mut value| {
                let boundary = self.boundary.get_or_insert_with(Boundary::new);
                let keep = boundary.upgrade(&mut value, &mut self.reporter)
                    && self.era.as_ref().map_or(true, |era| {
                        era.upgrade_event(&mut value, &mut self.reporter)
                    });
                // What this boundary keeps is charged with the processor's
                // own id bookkeeping (D28).
                processor.charge_boundary(boundary.retained());
                if keep {
                    processor.push_value(value, &mut self.reporter, &mut self.out)
                } else {
                    Ok(())
                }
            })
        };
        self.reporter.end_event(None);
        for processed in self.out.drain(..) {
            note(
                &processed.event,
                &self.run_id,
                &mut self.started,
                &mut self.last_ids,
                &mut self.terminals,
            );
            self.ready.push_back(processed);
        }
        result
    }

    /// Hands one processed event to observers and subscribers.
    fn deliver(&mut self, processed: Processed) -> Result<RunUpdate> {
        let update = RunUpdate {
            event: processed.event,
            changes: processed.changes,
        };
        let mut panicked = false;
        if let Some(processor) = &self.processor {
            if !self.config.observers.is_empty() {
                let reducer = processor.reducer();
                let view = RunView::new(
                    RunInfo::new(&self.thread_id, &self.run_id),
                    reducer.messages(),
                    reducer.state(),
                    reducer.pending_tool_calls(),
                );
                for observer in &self.config.observers {
                    let call = catch_unwind(AssertUnwindSafe(|| observer.on_event(&update, view)));
                    panicked |= call.is_err();
                }
            }
        }
        if !panicked && !self.config.subscribers.is_empty() {
            let mut mutation = Mutation::default();
            for subscriber in &self.config.subscribers {
                match catch_unwind(AssertUnwindSafe(|| {
                    subscriber.dispatch(&update.event, &mut mutation)
                })) {
                    Ok(Flow::Stop) => break,
                    Ok(_) => {}
                    Err(_) => {
                        panicked = true;
                        break;
                    }
                }
            }
            if let Some(state) = mutation.take_state() {
                self.state_override = Some(state);
            }
            if let Some(messages) = mutation.take_messages() {
                self.messages_override = Some(messages);
            }
        }
        if panicked || self.sink.observer_panicked.load(Ordering::Relaxed) {
            return Err(self.conclude_err(observer_panicked()));
        }
        Ok(update)
    }

    /// The frame stream ended: complete, truncated (TRN-09, TRN-10), or
    /// complete without the requested run (RUNIN-06).
    fn end_of_stream(&mut self) -> Option<Error> {
        let termination = match self.processor.as_mut().map(Processor::finish_checked) {
            Some(Ok(termination)) => termination,
            Some(Err(err)) => return Some(self.conclude_err(err)),
            None => Termination::Truncated,
        };
        if termination == Termination::Complete && self.terminals.last.is_some() {
            self.conclude(Ok(()))
        } else {
            Some(self.conclude_err(Error::new(
                ErrorCode::Truncated,
                "the stream ended before the run's terminal event",
            )))
        }
    }

    fn conclude_err(&mut self, err: Error) -> Error {
        match self.conclude(Err(err)) {
            Some(err) => err,
            None => Error::new(ErrorCode::Internal, "run concluded without an error"),
        }
    }

    /// Ends the run: builds the report, tells every observer exactly once.
    /// Returns the error to surface, if any.
    fn conclude(&mut self, outcome: std::result::Result<(), Error>) -> Option<Error> {
        if self.finished {
            return outcome.err();
        }
        self.finished = true;
        self.phase = Phase::Done;
        self.cancelled = None;
        self.ready.clear();
        // A panic in the final poll (a diagnostic raised there) fails the
        // run: observers are told the same failure the caller gets.
        let outcome = match outcome {
            Ok(()) if self.sink.observer_panicked.load(Ordering::Relaxed) => {
                Err(observer_panicked())
            }
            outcome => outcome,
        };
        match outcome {
            Ok(()) => {
                let report = self.build_report(None);
                let run = RunInfo::new(&self.thread_id, &self.run_id);
                let panicked = notify(&self.config, Ok(&report), run);
                if panicked || self.sink.observer_panicked.load(Ordering::Relaxed) {
                    Some(observer_panicked().with_partial(report))
                } else {
                    self.report = Some(report);
                    None
                }
            }
            Err(err) => {
                let err = if self.started {
                    let report = self.build_report(Some(&err));
                    err.with_partial(report)
                } else {
                    err
                };
                let run = RunInfo::new(&self.thread_id, &self.run_id);
                notify(&self.config, Err(&err), run);
                Some(err)
            }
        }
    }

    /// Moves the history out of the reducer (no copy).
    fn build_report(&mut self, failure: Option<&Error>) -> RunReport {
        let (mut messages, mut state, usage, interrupts, pending, new_ids) =
            match self.processor.take() {
                Some(processor) => {
                    let reducer = processor.into_reducer();
                    let usage = reducer.usage().to_vec();
                    let interrupts = reducer.open_interrupts().to_vec();
                    let pending = reducer.pending_tool_calls().to_vec();
                    let new_ids = reducer.run_message_ids().to_vec();
                    let (messages, state) = reducer.into_parts();
                    (messages, state, usage, interrupts, pending, new_ids)
                }
                None => (
                    Vec::new(),
                    empty_object(),
                    Vec::new(),
                    Vec::new(),
                    Vec::new(),
                    Vec::new(),
                ),
            };
        if let Some(replacement) = self.messages_override.take() {
            messages = replacement;
        }
        if let Some(replacement) = self.state_override.take() {
            state = replacement;
        }
        // A partial report of a run with no terminal event: never a
        // success (TRN-10); it carries the client-side failure.
        let unterminated = || match failure {
            Some(err) => {
                RunStatus::Errored(RunError::new(err.message()).with_code(err.code().as_str()))
            }
            None => RunStatus::Errored(
                RunError::new("the run reached no terminal event")
                    .with_code(ErrorCode::Truncated.as_str()),
            ),
        };
        let requested_is_last = self.terminals.last_is_requested;
        let status = self.terminals.last.take().unwrap_or_else(unterminated);
        let (thread_id, run_id) = self
            .last_ids
            .take()
            .unwrap_or_else(|| (self.thread_id.clone(), self.run_id.clone()));
        // The requested run's own ending: an earlier run on a multi-run
        // stream keeps its verdict, so a failed requested run is never
        // reported as a success because a later run succeeded (LIFE-23).
        // Held once while the run was open; copied only here, when both
        // report fields need it.
        let requested_status = match self.terminals.requested.take() {
            _ if requested_is_last => status.clone(),
            Some(requested) => requested,
            None if run_id == self.run_id => status.clone(),
            None => unterminated(),
        };
        RunReport {
            thread_id,
            run_id,
            status,
            requested_run_id: self.run_id.clone(),
            requested_status,
            messages,
            new_message_ids: new_ids,
            state,
            usage,
            interrupts,
            pending_tool_calls: pending,
            diagnostics: self.sink.take(),
        }
    }
}

impl Drop for RunDriver {
    /// The drop guard: a run dropped before its end still tells every
    /// observer, once, that it ended (`run.cancelled`).
    fn drop(&mut self) {
        if !self.finished {
            let _ = self.conclude(Err(Error::new(
                ErrorCode::Cancelled,
                "the run handle was dropped before the run ended",
            )));
        }
        if self.owns_cancel {
            // Wakes a transport still parked on the token.
            self.cancel.cancel();
        }
    }
}

/// Tracks lifecycle facts as events leave the processor: the last run's
/// ids and terminal, and the terminal of the run the request named.
fn note(
    event: &Event,
    requested: &RunId,
    started: &mut bool,
    last_ids: &mut Option<(ThreadId, RunId)>,
    terminals: &mut Terminals,
) {
    let status = match event {
        Event::RunStarted(e) => {
            *started = true;
            *last_ids = Some((e.thread_id.clone(), e.run_id.clone()));
            terminals.keep_requested();
            terminals.last = None;
            return;
        }
        Event::RunFinished(e) => {
            let success = matches!(e.outcome, None | Some(RunFinishedOutcome::Success { .. }));
            RunStatus::Finished {
                outcome: e.outcome.clone(),
                // The result is the value of a completed run only.
                result: if success { e.result.clone() } else { None },
            }
        }
        Event::RunError(e) => RunStatus::Errored(e.clone()),
        _ => return,
    };
    // A stream that opens with RUN_ERROR is the requested run failing
    // before it started (LIFE-03).
    let is_requested = last_ids.as_ref().map_or(true, |(_, run)| run == requested);
    terminals.keep_requested();
    if is_requested {
        terminals.requested = None;
    }
    terminals.last = Some(status);
    terminals.last_is_requested = is_requested;
}

/// How the last run and the requested run ended. A status (whose `result`
/// may be large) is held once: when the last run is the requested one,
/// `last` stands for both.
#[derive(Default)]
struct Terminals {
    /// The last run's terminal; `None` while it runs.
    last: Option<RunStatus>,
    /// The requested run's terminal, when it is not `last`.
    requested: Option<RunStatus>,
    /// `last` is the requested run's terminal.
    last_is_requested: bool,
}

impl Terminals {
    /// The last run ended, and so did the requested run.
    fn requested_concluded(&self) -> bool {
        self.last.is_some() && (self.last_is_requested || self.requested.is_some())
    }

    /// `last` is about to be replaced: keep it if it is the requested run's.
    fn keep_requested(&mut self) {
        if std::mem::take(&mut self.last_is_requested) {
            self.requested = self.last.take();
        }
    }
}

/// `on_finish` to every observer; true if one panicked.
fn notify(
    config: &ClientConfig,
    result: std::result::Result<&RunReport, &Error>,
    run: RunInfo<'_>,
) -> bool {
    let mut panicked = false;
    for observer in &config.observers {
        panicked |= catch_unwind(AssertUnwindSafe(|| observer.on_finish(result, run))).is_err();
    }
    panicked
}

/// Transport failures that, once the requested run's terminal arrived, end
/// the stream rather than fail the run (a timeout, a read error, a body cut
/// inside the next event). Framing and protocol errors still fail it.
fn ends_the_stream(code: ErrorCode) -> bool {
    matches!(
        code,
        ErrorCode::Timeout | ErrorCode::IdleTimeout | ErrorCode::StreamRead | ErrorCode::Truncated
    )
}

fn cancelled_error() -> Error {
    Error::new(ErrorCode::Cancelled, "the run was cancelled")
}

fn observer_panicked() -> Error {
    Error::new(
        ErrorCode::ObserverPanicked,
        "an observer, subscriber or the diagnostic sink panicked; the run was failed",
    )
}

fn middleware_panicked() -> Error {
    Error::new(ErrorCode::MiddlewareFailed, "a middleware panicked")
}

fn transport_panicked() -> Error {
    Error::new(ErrorCode::StreamRead, "the transport panicked")
}

fn empty_object() -> Value {
    Value::Object(serde_json::Map::new())
}

fn empty_state() -> &'static Value {
    static EMPTY: OnceLock<Value> = OnceLock::new();
    EMPTY.get_or_init(empty_object)
}

fn to_value(input: &RunAgentInput) -> Result<Value> {
    serde_json::to_value(input).map_err(|err| {
        Error::new(ErrorCode::Internal, "run input did not serialize").with_source(err)
    })
}

/// The frame guard: a frame over the size budget is refused before any
/// parse, whatever transport produced it.
fn guard_frame(frame: &Frame, max_frame_bytes: usize) -> Result<()> {
    match frame.raw_len() {
        Some(len) if len > max_frame_bytes => Err(Error::new(
            ErrorCode::FrameTooLarge,
            format!("an event frame of {len} bytes exceeds the {max_frame_bytes}-byte limit"),
        )),
        _ => Ok(()),
    }
}

/// Per-run headers: reserved names and header injection are refused before
/// anything is sent.
fn check_headers(headers: &[Header]) -> Result<()> {
    for header in headers {
        let name = header.name.as_str();
        let token = !name.is_empty()
            && name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b));
        if !token {
            return Err(Error::new(
                ErrorCode::InvalidRequest,
                "a per-run header name is not a valid HTTP token",
            ));
        }
        if RESERVED_HEADERS
            .iter()
            .any(|r| name.eq_ignore_ascii_case(r))
        {
            return Err(Error::new(
                ErrorCode::InvalidRequest,
                format!("the per-run header `{name}` is reserved"),
            ));
        }
        if header
            .value
            .bytes()
            .any(|b| b == b'\r' || b == b'\n' || b == 0)
        {
            return Err(Error::new(
                ErrorCode::InvalidRequest,
                format!("the per-run header `{name}` has a control character in its value"),
            ));
        }
    }
    Ok(())
}

/// What the transport request needs besides the body.
struct RequestParts {
    thread_id: ThreadId,
    run_id: RunId,
    headers: Vec<Header>,
    cancel: CancelToken,
    sink: Arc<dyn DiagnosticSink>,
    max_frame_bytes: usize,
    max_container_entries: usize,
}

impl RequestParts {
    fn into_request(self, body: Chunk) -> TransportRequest {
        TransportRequest {
            body,
            thread_id: self.thread_id,
            run_id: self.run_id,
            headers: self.headers,
            accept: accept(),
            cancel: self.cancel,
            diagnostics: self.sink,
            max_frame_bytes: self.max_frame_bytes,
            max_container_entries: self.max_container_entries,
        }
    }
}

/// Wire formats this build reads, preferred first; SSE always (PB-06).
fn accept() -> Vec<WireFormat> {
    #[cfg(feature = "proto")]
    {
        vec![WireFormat::Protobuf, WireFormat::Sse]
    }
    #[cfg(not(feature = "proto"))]
    {
        vec![WireFormat::Sse]
    }
}

/// Builds the chain, first registered outermost (PROC-21). A middleware that
/// panics while building its stream fails the run with `middleware.failed`.
fn run_chain(
    middleware: &[Arc<dyn Middleware>],
    request: MiddlewareRequest,
    innermost: Next,
) -> FrameStream {
    let mut next = innermost;
    for stage in middleware.iter().rev() {
        let stage = Arc::clone(stage);
        let inner = next;
        next = Next::from_fn(move |request| {
            catch_unwind(AssertUnwindSafe(|| stage.run(request, inner)))
                .unwrap_or_else(|_| FrameStream::from_frames([Err(middleware_panicked())]))
        });
    }
    next.run(request)
}

/// The innermost stage on the middleware path: enforces the (possibly
/// rewritten) input, sends it, and hands frames up through the boundary and
/// era shims, so middleware sees 1.0 shapes (VER-14, PROC-22).
fn upstream(
    open: Open,
    parts: RequestParts,
    era: Option<EraShims>,
    sink: Arc<RunSink>,
    limits: crate::pipeline::Limits,
    retained: Arc<AtomicUsize>,
) -> Next {
    Next::from_fn(move |request: MiddlewareRequest| {
        let sink = sink as Arc<dyn DiagnosticSink>;
        let mut reporter = Reporter::new(Arc::clone(&sink), limits.max_diagnostics_per_event);
        let body = outgoing::finish_value(request.input, &Enforcer::new(limits), &mut reporter);
        reporter.end_event(None);
        let body = match body {
            Ok(body) => body,
            Err(err) => return FrameStream::from_frames([Err(err)]),
        };
        FrameStream::new(Upstream {
            state: UpstreamState::Opening(open(parts.into_request(body))),
            boundary: Boundary::new(),
            retained: Arc::clone(&retained),
            charged: 0,
            era,
            reporter,
            max_frame_bytes: limits.max_frame_bytes,
            max_history_bytes: limits.max_history_bytes,
            enforcer: Enforcer::new(limits),
        })
    })
}

enum UpstreamState {
    Opening(OpenFuture),
    Streaming(FrameStream),
    Done,
}

struct Upstream {
    state: UpstreamState,
    boundary: Boundary,
    /// The driver's sum of what live `Upstream` boundaries keep; this
    /// stage's share of it is `charged`.
    retained: Arc<AtomicUsize>,
    charged: usize,
    era: Option<EraShims>,
    reporter: Reporter,
    max_frame_bytes: usize,
    /// The boundary's own ceiling (D28). Its share is also charged to the
    /// processor on every frame that reaches it, but a middleware that drops
    /// the translated frames would otherwise let the table grow unchecked.
    max_history_bytes: usize,
    /// Parses raw frames with the container-entry and depth budgets applied
    /// as the tree is built.
    enforcer: Enforcer,
}

impl Upstream {
    fn upgrade(&mut self, frame: Frame) -> Result<Option<Frame>> {
        guard_frame(&frame, self.max_frame_bytes)?;
        let mut value = match frame.as_json() {
            Some(bytes) => match self.enforcer.parse_frame(bytes)? {
                value @ Value::Object(_) => value,
                _ => {
                    return Err(Error::new(
                        ErrorCode::MalformedEvent,
                        "event frame is not a JSON object",
                    ))
                }
            },
            None => frame.into_value()?,
        };
        let keep = self.boundary.upgrade(&mut value, &mut self.reporter)
            && self.era.as_ref().map_or(true, |era| {
                era.upgrade_event(&mut value, &mut self.reporter)
            });
        self.reporter.end_event(None);
        let retained = self.boundary.retained();
        self.recharge(retained);
        if retained > self.max_history_bytes {
            return Err(Error::new(
                ErrorCode::LimitExceeded,
                "the compatibility boundary's open spans exceed the history limit",
            ));
        }
        Ok(if keep {
            Some(Frame::from_value(value))
        } else {
            None
        })
    }
}

impl Upstream {
    /// Moves this stage's share of `retained` to `now`.
    fn recharge(&mut self, now: usize) {
        if now >= self.charged {
            self.retained
                .fetch_add(now - self.charged, Ordering::Relaxed);
        } else {
            self.retained
                .fetch_sub(self.charged - now, Ordering::Relaxed);
        }
        self.charged = now;
    }
}

impl Drop for Upstream {
    fn drop(&mut self) {
        self.recharge(0);
    }
}

impl FrameSource for Upstream {
    fn poll_frame(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Result<Frame>>> {
        let this = self.get_mut();
        // Each source has its own per-call budget of dropped frames; a flood
        // of them yields to the executor after `FRAMES_PER_POLL` drops.
        let mut dropped = 0usize;
        loop {
            // A transport panic is caught here, not by the driver's
            // middleware guard, so it is reported as the transport's
            // (`transport_panicked`); the state then goes to `Done`, so the
            // poisoned future or stream is never polled again.
            let polled = match &mut this.state {
                UpstreamState::Opening(future) => {
                    match catch_unwind(AssertUnwindSafe(|| future.as_mut().poll(cx)))
                        .unwrap_or_else(|_| Poll::Ready(Err(transport_panicked())))
                    {
                        Poll::Ready(Ok(stream)) => {
                            this.state = UpstreamState::Streaming(stream);
                            continue;
                        }
                        Poll::Ready(Err(err)) => Some(Err(err)),
                        Poll::Pending => return Poll::Pending,
                    }
                }
                UpstreamState::Streaming(stream) => {
                    match catch_unwind(AssertUnwindSafe(|| stream.poll_next(cx)))
                        .unwrap_or_else(|_| Poll::Ready(Some(Err(transport_panicked()))))
                    {
                        Poll::Ready(polled) => polled,
                        Poll::Pending => return Poll::Pending,
                    }
                }
                UpstreamState::Done => return Poll::Ready(None),
            };
            match polled {
                Some(Ok(frame)) => match this.upgrade(frame) {
                    Ok(Some(frame)) => return Poll::Ready(Some(Ok(frame))),
                    Ok(None) => {
                        dropped += 1;
                        if dropped >= FRAMES_PER_POLL {
                            cx.waker().wake_by_ref();
                            return Poll::Pending;
                        }
                        continue;
                    }
                    Err(err) => {
                        this.state = UpstreamState::Done;
                        return Poll::Ready(Some(Err(err)));
                    }
                },
                Some(Err(err)) => {
                    this.state = UpstreamState::Done;
                    return Poll::Ready(Some(Err(err)));
                }
                None => {
                    this.state = UpstreamState::Done;
                    return Poll::Ready(None);
                }
            }
        }
    }
}
