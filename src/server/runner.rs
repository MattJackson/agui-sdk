use std::collections::{HashSet, VecDeque};
use std::future::Future;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use serde_json::Value;

use crate::cancel::{CancelToken, Cancelled};
use crate::diagnostics::{
    default_sink, Diagnostic, DiagnosticCode, DiagnosticSink, GuardedSink, Reporter, Scope,
    DEFAULT_MAX_PER_EVENT,
};
use crate::error::{BoxError, Error, ErrorCode, Result};
use crate::event::Event;
use crate::ids::{InterruptId, RunId, ThreadId};
use crate::interrupt::Interrupt;
use crate::lifecycle::{RunError, RunFinished, RunFinishedOutcome, RunStarted};
use crate::message::Message;
use crate::pipeline::compat::EraShims;
use crate::pipeline::verify::ProducerVerifier;
use crate::state::{MessagesSnapshot, StateSnapshot};
use crate::subagent::{SubagentError, SubagentFinishedOutcome};
use crate::usage::TokenUsage;
use crate::version::{LegacyEra, PeerVersion};

use super::agent::OutcomeKind;
use super::emit::{Emitter, Failure, RunSetup, RunShared};
use super::input::AdmittedInput;
use super::state::StatePublisher;
use super::{Agent, Extensions, RunContext, RunOutcome};

/// Runner settings. Defaults are the safe ones.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct RunnerConfig {
    /// Events buffered between the agent and the encoder. Default 1024. A
    /// full queue suspends the agent's `emit` (backpressure); it never grows.
    pub queue_capacity: usize,
    /// Catch agent panics and end the run with `RUN_ERROR`. Default on.
    pub catch_unwind: bool,
    /// Echo the input on `RUN_STARTED.input` (RUNIN-02). Default off: input
    /// can carry secrets.
    pub echo_input: bool,
    /// The era to downgrade the stream to for a client that sends no
    /// `protocolVersion` (VER-03..VER-09). The runner drops image, audio,
    /// video and document parts for eras up to 0.0.47 (with a warning) rather
    /// than converting them to legacy `binary`. A missing declaration does not identify an era, so
    /// this is configuration, never a guess: `None` (the default) serves such
    /// clients as current (VER-31). Clients that declare a version are never
    /// downgraded.
    pub legacy_peer: Option<LegacyEra>,
}

impl Default for RunnerConfig {
    fn default() -> Self {
        Self {
            queue_capacity: 1024,
            catch_unwind: true,
            echo_input: false,
            legacy_peer: None,
        }
    }
}

/// Rewrites agent events before verification (e.g. redaction, provider
/// translation). Synchronous; appends zero or more events to `out`.
/// A transformer's output is verified like the agent's own.
pub trait Transformer: Send + 'static {
    /// Transforms one event.
    fn transform(&mut self, event: Event, out: &mut Vec<Event>);
}

type TransformerFactory = Arc<dyn Fn() -> Box<dyn Transformer> + Send + Sync>;

/// An earlier run restated ahead of the requested one, for a server that
/// replays a thread (LIFE-43, RUNIN-31, SSE-11). Restated as snapshots, never
/// re-streamed (LIFE-46).
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub struct ReplayedRun {
    /// The earlier run's own id (RUNIN-06).
    pub run_id: RunId,
    /// History as of the end of that run.
    pub messages: Vec<Message>,
    /// State as of the end of that run.
    pub state: Value,
    /// How it ended.
    pub outcome: Option<RunFinishedOutcome>,
}

impl ReplayedRun {
    /// A replayed run.
    #[must_use]
    pub fn new(run_id: RunId, messages: Vec<Message>, state: Value) -> Self {
        Self {
            run_id,
            messages,
            state,
            outcome: None,
        }
    }
}

/// Drives an [`Agent`] for admitted requests. Cheap to clone.
///
/// The runner opens and closes the run itself and verifies every event the
/// agent sends. Without a framework, admit the request body and poll the
/// stream (any executor works; the server needs no runtime):
///
/// ```
/// # use agui_sdk::server::{Agent, BoxError, RunContext, RunOutcome};
/// #
/// # struct Echo;
/// #
/// # impl Agent for Echo {
/// #     type State = serde_json::Value;
/// #
/// #     async fn run(&self, ctx: RunContext) -> Result<RunOutcome, BoxError> {
/// #         ctx.emitter().say("Hello").await?;
/// #         Ok(RunOutcome::success())
/// #     }
/// # }
/// use agui_sdk::server::input::{admit, InputPolicy};
/// use agui_sdk::server::{CancelToken, Extensions, Runner};
/// use agui_sdk::EventType;
///
/// let input = admit(br#"{"threadId":"t-1","runId":"r-1","messages":[]}"#, &InputPolicy::default())?;
/// let runner = Runner::new(Echo);
/// let mut stream = runner.run(input, CancelToken::new(), Extensions::default())?;
///
/// let mut types = Vec::new();
/// futures_executor::block_on(async {
///     while let Some(event) = stream.next().await {
///         types.push(event.event_type());
///     }
/// });
/// assert_eq!(types.first(), Some(&EventType::RunStarted));
/// assert_eq!(types.last(), Some(&EventType::RunFinished));
/// # Ok::<(), agui_sdk::Error>(())
/// ```
pub struct Runner<A> {
    agent: Arc<A>,
    config: RunnerConfig,
    transformers: Vec<TransformerFactory>,
    diagnostics: Option<Arc<dyn DiagnosticSink>>,
}

impl<A: Agent> Runner<A> {
    /// A runner with default settings.
    pub fn new(agent: A) -> Self {
        Self {
            agent: Arc::new(agent),
            config: RunnerConfig::default(),
            transformers: Vec::new(),
            diagnostics: None,
        }
    }

    /// Replaces the settings.
    #[must_use]
    pub fn config(self, config: RunnerConfig) -> Self {
        Self { config, ..self }
    }

    /// Adds a transformer; `factory` makes one per run.
    #[must_use]
    pub fn transformer<T, F>(self, factory: F) -> Self
    where
        T: Transformer,
        F: Fn() -> T + Send + Sync + 'static,
    {
        let mut this = self;
        this.transformers.push(Arc::new(move || {
            Box::new(factory()) as Box<dyn Transformer>
        }));
        this
    }

    /// Where diagnostics go (default: [`default_sink`](crate::diagnostics::default_sink)).
    #[must_use]
    pub fn diagnostic_sink(self, sink: impl DiagnosticSink + 'static) -> Self {
        Self {
            diagnostics: Some(Arc::new(sink)),
            ..self
        }
    }

    /// Runs one admitted request. The stream yields verified events and ends
    /// after exactly one terminal event, whatever the agent does.
    /// `extensions` carries request-scoped data (principal, tenant, trace
    /// context) to [`RunContext::extensions`](super::RunContext::extensions).
    ///
    /// # Errors
    ///
    /// Before any stream exists: [`ErrorCode::InputRejected`]
    /// when the input's state does not deserialize into `A::State` (an
    /// absent state is read as `{}`, RUNIN-24). Sent as HTTP 400, never as
    /// `RUN_ERROR`.
    pub fn run(
        &self,
        input: AdmittedInput,
        cancel: CancelToken,
        extensions: Extensions,
    ) -> Result<RunStream> {
        self.run_with_replay(Vec::new(), input, cancel, extensions)
    }

    /// As [`run`](Self::run), restating `replay` first.
    ///
    /// # Errors
    ///
    /// As [`run`](Self::run).
    pub fn run_with_replay(
        &self,
        replay: Vec<ReplayedRun>,
        input: AdmittedInput,
        cancel: CancelToken,
        extensions: Extensions,
    ) -> Result<RunStream> {
        let AdmittedInput {
            input,
            peer_version,
            diagnostics,
        } = input;
        // The sink is user code: a panic in it fails the run (`RUN_ERROR`,
        // `agent.panicked`) instead of unwinding out of `poll_next`.
        let sink = Arc::new(GuardedSink::new(
            self.diagnostics.clone().unwrap_or_else(default_sink),
        ));
        let mut startup_failure = None;
        // A state that does not fit the agent's type is bad input: the
        // transport's 400, before any stream exists.
        let state = decode_state::<A::State>(input.state.as_ref())?;
        let transformers = match catch_unwind(AssertUnwindSafe(|| {
            self.transformers
                .iter()
                .map(|make| make())
                .collect::<Vec<_>>()
        })) {
            Ok(transformers) => transformers,
            Err(_) => {
                startup_failure = Some(Failure {
                    code: ErrorCode::AgentPanicked,
                    message: "a transformer factory panicked".to_owned(),
                });
                Vec::new()
            }
        };

        // Only an undeclared client is downgraded, and only to the configured
        // era: a missing declaration is never a guess (VER-30, VER-31).
        let downgrade = match (self.config.legacy_peer, &peer_version) {
            (Some(era), PeerVersion::Absent) => Some(EraShims::new(era)),
            _ => None,
        };

        let mut started = RunStarted::new(input.thread_id.clone(), input.run_id.clone());
        if downgrade.is_none() {
            // Our own version, never an echo of the input's (VER-18).
            started.protocol_version = Some(crate::PROTOCOL_VERSION.to_owned());
        }
        started.parent_run_id = input.parent_run_id.clone();
        if self.config.echo_input {
            started.input = Some(Box::new(input.clone()));
        }

        let publisher = StatePublisher::new(input.state.clone().unwrap_or(Value::Null));
        let frontend_tools: HashSet<String> =
            input.tools().iter().map(|t| t.name.clone()).collect();
        let verifier = ProducerVerifier::for_input(&input);
        let shared = RunShared::new(RunSetup {
            capacity: self.config.queue_capacity,
            transformers,
            publisher,
            frontend_tools,
            verifier,
            cancel: cancel.clone(),
        });
        let emitter = Emitter::new(Arc::clone(&shared));
        let ids = (input.thread_id.clone(), input.run_id.clone());
        let cancelled = cancel.cancelled();
        let ctx = RunContext::new(input, peer_version, state, emitter, cancel, extensions);

        let driver = Driver {
            agent: Arc::clone(&self.agent),
            shared,
            phase: Phase::Start,
            ctx: Some(ctx),
            future: None,
            tail: VecDeque::new(),
            replay,
            started: Some(started),
            startup_failure,
            admission: diagnostics,
            reporter: Reporter::new(
                Arc::clone(&sink) as Arc<dyn DiagnosticSink>,
                DEFAULT_MAX_PER_EVENT,
            ),
            sink,
            catch_unwind: self.config.catch_unwind,
            downgrade,
            ids,
            cancelled,
        };
        Ok(RunStream {
            driver: Box::pin(RunDriver {
                inner: Box::new(driver),
            }),
        })
    }
}

impl<A> Clone for Runner<A> {
    fn clone(&self) -> Self {
        let Self {
            agent,
            config,
            transformers,
            diagnostics,
        } = self;
        Self {
            agent: Arc::clone(agent),
            config: config.clone(),
            transformers: transformers.clone(),
            diagnostics: diagnostics.clone(),
        }
    }
}

impl<A> std::fmt::Debug for Runner<A> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut out = f.debug_struct("Runner");
        out.field("config", &self.config);
        out.field("transformers", &self.transformers.len());
        out.field("custom_sink", &self.diagnostics.is_some());
        out.finish_non_exhaustive()
    }
}

/// The verified event stream of one request. Polling it drives the agent:
/// no task is spawned and no runtime is required. Consume it with
/// [`next`](Self::next) / [`poll_next`](Self::poll_next), or as a
/// `futures_core::Stream` with feature `stream`.
#[must_use = "the agent runs only while its stream is polled; dropping it ends the run"]
pub struct RunStream {
    driver: Pin<Box<RunDriver>>,
}

/// The agent future, its queue, transformers, verifier and publisher.
struct RunDriver {
    inner: Box<dyn Drive>,
}

/// The driver with the agent type erased: one virtual call per poll.
trait Drive: Send {
    fn poll_event(&mut self, cx: &mut Context<'_>) -> Poll<Option<Event>>;
}

impl RunStream {
    /// Polls for the next event.
    pub fn poll_next(&mut self, cx: &mut Context<'_>) -> Poll<Option<Event>> {
        self.driver.inner.poll_event(cx)
    }

    /// The next event; `None` after the terminal event.
    // An inherent async `next`, like an async iterator; not `Iterator::next`.
    #[allow(clippy::should_implement_trait)]
    pub fn next(&mut self) -> impl Future<Output = Option<Event>> + Send + '_ {
        NextEvent { stream: self }
    }
}

struct NextEvent<'a> {
    stream: &'a mut RunStream,
}

impl Future for NextEvent<'_> {
    type Output = Option<Event>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Event>> {
        self.get_mut().stream.poll_next(cx)
    }
}

#[cfg(feature = "stream")]
#[cfg_attr(docsrs, doc(cfg(feature = "stream")))]
impl futures_core::Stream for RunStream {
    type Item = Event;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Event>> {
        RunStream::poll_next(self.get_mut(), cx)
    }
}

impl std::fmt::Debug for RunStream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RunStream").finish_non_exhaustive()
    }
}

// ---------------------------------------------------------------------------
// The driver: the state machine behind `RunStream`. Polling it polls the
// agent's future, drains the run queue, and, once the run is decided, emits
// the closers and exactly one terminal event.
// ---------------------------------------------------------------------------

type AgentFuture = Pin<Box<dyn Future<Output = Result<RunOutcome, BoxError>> + Send>>;

enum Phase {
    /// Nothing sent yet.
    Start,
    /// The agent is running.
    Running,
    /// The run is decided; drain the queue, then close.
    Finishing(Ending),
    /// The terminal is in `tail`; nothing follows it.
    Done,
}

/// How the run was decided.
enum Ending {
    Agent(Result<RunOutcome, BoxError>),
    Panicked,
    Cancelled,
    Failed(Failure),
}

struct Driver<A: Agent> {
    agent: Arc<A>,
    shared: Arc<RunShared>,
    phase: Phase,
    ctx: Option<RunContext<A::State>>,
    future: Option<AgentFuture>,
    /// Runner-made events waiting to be yielded, already verified.
    tail: VecDeque<Event>,
    replay: Vec<ReplayedRun>,
    started: Option<RunStarted>,
    startup_failure: Option<Failure>,
    admission: Vec<Diagnostic>,
    reporter: Reporter,
    /// The reporter's sink, to see whether it panicked.
    sink: Arc<GuardedSink>,
    catch_unwind: bool,
    downgrade: Option<EraShims>,
    ids: (ThreadId, RunId),
    /// Polled when the agent is suspended, so a cancel wakes the stream.
    cancelled: Cancelled,
}

// The driver never pins its fields in place (the agent future is boxed), so
// it is `Unpin` whatever the agent's state type is.
impl<A: Agent> Unpin for Driver<A> {}

impl<A: Agent> Drop for Driver<A> {
    fn drop(&mut self) {
        // A dropped stream is a disconnect: later sends fail instead of
        // waiting forever for space. Closed first, so a send blocked on a
        // full queue is failed and woken before agent destructors run (one
        // may wait for the task making that send).
        self.shared.close();
        // Dropping the agent's future and context may run agent destructors
        // (a disconnect mid-run); as in `decide`, a panic there must not
        // escape the stream's drop.
        let future = self.future.take();
        let ctx = self.ctx.take();
        let _ = catch_unwind(AssertUnwindSafe(move || drop((future, ctx))));
    }
}

impl<A: Agent> Drive for Driver<A> {
    fn poll_event(&mut self, cx: &mut Context<'_>) -> Poll<Option<Event>> {
        let this = self;
        loop {
            if let Some(event) = this.tail.pop_front() {
                match this.output(event) {
                    Some(event) => return Poll::Ready(Some(event)),
                    None => continue,
                }
            }
            match std::mem::replace(&mut this.phase, Phase::Done) {
                Phase::Start => this.start(),
                Phase::Running => {
                    this.phase = Phase::Running;
                    if let Some(event) = this.shared.poll_pop(cx.waker()) {
                        if let Some(event) = this.output(event) {
                            return Poll::Ready(Some(event));
                        }
                        continue;
                    }
                    if let Some(failure) = this.shared.failure() {
                        this.decide(Ending::Failed(failure));
                        continue;
                    }
                    if this.shared.cancel.is_cancelled() {
                        this.decide(Ending::Cancelled);
                        continue;
                    }
                    match this.poll_agent(cx) {
                        Poll::Ready(ending) => {
                            this.decide(ending);
                            continue;
                        }
                        Poll::Pending => {}
                    }
                    // Registers the waker with the token; ready means cancelled.
                    let cancelled = Pin::new(&mut this.cancelled).poll(cx).is_ready();
                    if cancelled || this.shared.failure().is_some() {
                        continue;
                    }
                    // The agent may have queued events before suspending.
                    if let Some(event) = this.shared.poll_pop(cx.waker()) {
                        if let Some(event) = this.output(event) {
                            return Poll::Ready(Some(event));
                        }
                        continue;
                    }
                    return Poll::Pending;
                }
                Phase::Finishing(ending) => {
                    // Everything queued before the decision was verified;
                    // deliver it ahead of the closers.
                    if let Some(event) = this.shared.poll_pop(cx.waker()) {
                        this.phase = Phase::Finishing(ending);
                        if let Some(event) = this.output(event) {
                            return Poll::Ready(Some(event));
                        }
                        continue;
                    }
                    let events = match catch_unwind(AssertUnwindSafe(|| this.terminal(ending))) {
                        Ok(events) => events,
                        Err(_) => vec![run_error(
                            ErrorCode::Internal,
                            "the run could not be closed",
                            None,
                        )],
                    };
                    this.tail.extend(events);
                    this.phase = Phase::Done;
                }
                Phase::Done => return Poll::Ready(None),
            }
        }
    }
}

impl<A: Agent> Driver<A> {
    /// Replays, opens the run, starts the agent.
    fn start(&mut self) {
        // Admission already capped its list and summarised what it dropped;
        // capping it again would hide that summary and what follows it.
        let admission = std::mem::take(&mut self.admission);
        if !admission.is_empty() && self.sink.enabled() {
            for diagnostic in &admission {
                self.sink.report(diagnostic);
            }
        }
        if self.sink.panicked() && self.startup_failure.is_none() {
            self.startup_failure = Some(sink_panicked());
        }

        let thread_id = match &self.started {
            Some(started) => started.thread_id.clone(),
            None => return,
        };
        let replayed = !self.replay.is_empty();
        for run in std::mem::take(&mut self.replay) {
            for event in replay_events(&thread_id, run, self.downgrade.is_none()) {
                if let Err(why) = self.push_verified(event) {
                    self.fail_now(
                        ErrorCode::ProducerViolation,
                        &format!("a replayed run was refused: {why}"),
                    );
                    return;
                }
            }
        }

        if let Some(failure) = self.startup_failure.take() {
            // The run fails before it begins (LIFE-01). After a replay the
            // stream already has runs, so open this one first.
            if replayed {
                if let Some(started) = self.started.take() {
                    if let Err(why) = self.push_verified(started.into()) {
                        // The run cannot even be opened: that is the error.
                        self.fail_now(
                            ErrorCode::ProducerViolation,
                            &format!("RUN_STARTED was refused: {why}"),
                        );
                        return;
                    }
                }
            }
            self.tail
                .push_back(run_error(failure.code, &failure.message, None));
            self.phase = Phase::Done;
            return;
        }

        if let Some(started) = self.started.take() {
            if let Err(why) = self.push_verified(started.into()) {
                self.fail_now(
                    ErrorCode::ProducerViolation,
                    &format!("RUN_STARTED was refused: {why}"),
                );
                return;
            }
        }

        let Some(ctx) = self.ctx.take() else {
            return;
        };
        let agent = Arc::clone(&self.agent);
        self.future = Some(Box::pin(async move { agent.run(ctx).await }));
        self.phase = Phase::Running;
    }

    /// Verifies a runner-made event and queues it for output.
    /// The refusal is the verifier's message (ids, no payloads).
    fn push_verified(&mut self, event: Event) -> Result<(), String> {
        let mut st = self.shared.lock();
        let verdict = st.verifier.check(&event);
        if verdict.is_ok() {
            // A replayed snapshot is what the client holds next: it is the
            // baseline for the run's first delta (RUNIN-25, SNAP-07).
            st.publisher.observe(&event);
        }
        drop(st);
        match verdict {
            Ok(()) => {
                self.tail.push_back(event);
                Ok(())
            }
            Err(violation) => Err(violation.message),
        }
    }

    fn fail_now(&mut self, code: ErrorCode, message: &str) {
        self.tail.push_back(run_error(code, message, None));
        self.phase = Phase::Done;
    }

    fn poll_agent(&mut self, cx: &mut Context<'_>) -> Poll<Ending> {
        let Some(future) = self.future.as_mut() else {
            return Poll::Ready(Ending::Failed(Failure {
                code: ErrorCode::Internal,
                message: "the agent was not started".to_owned(),
            }));
        };
        let polled = if self.catch_unwind {
            catch_unwind(AssertUnwindSafe(|| future.as_mut().poll(cx)))
        } else {
            Ok(future.as_mut().poll(cx))
        };
        match polled {
            Ok(Poll::Ready(result)) => Poll::Ready(Ending::Agent(result)),
            Ok(Poll::Pending) => Poll::Pending,
            Err(_) => Poll::Ready(Ending::Panicked),
        }
    }

    /// The run is decided: stop the agent and refuse further sends.
    fn decide(&mut self, ending: Ending) {
        // Closed first, as in `Drop`: a send blocked on a full queue is
        // failed and woken before agent destructors run (one may wait for
        // the task making that send).
        self.shared.close();
        // Dropping the future may run agent destructors; a panic there must
        // not escape the stream either.
        let future = self.future.take();
        let _ = catch_unwind(AssertUnwindSafe(move || drop(future)));
        self.phase = Phase::Finishing(ending);
    }

    /// A terminal `RUN_ERROR` passed through the producer verifier (ARCH-04).
    fn verified(&self, event: Event, usage: Option<Vec<TokenUsage>>) -> Event {
        let mut st = self.shared.lock();
        verified_terminal(&mut st.verifier, event, usage)
    }

    /// Closers and the one terminal event.
    fn terminal(&mut self, ending: Ending) -> Vec<Event> {
        let usage = std::mem::take(&mut self.shared.lock().usage);
        let usage = (!usage.is_empty()).then_some(usage);
        let cancelled = self.shared.cancel.is_cancelled();
        // A refused send, an overflow or a transformer panic decides the run
        // however the agent itself ended (the agent was told at its call site).
        let ending = match self.shared.failure() {
            Some(failure) => Ending::Failed(failure),
            None => ending,
        };
        let (outcome, result) = match ending {
            Ending::Failed(failure) => {
                let event = run_error(failure.code, &failure.message, usage.clone());
                return vec![self.verified(event, usage)];
            }
            Ending::Panicked => {
                let event = run_error(
                    ErrorCode::AgentPanicked,
                    "the agent panicked",
                    usage.clone(),
                );
                return vec![self.verified(event, usage)];
            }
            Ending::Cancelled => (Close::Cancelled, None),
            // A stopped run is never reported as completed, waiting or
            // failed by the agent (LIFE-11, LIFE-16), however it wound down.
            Ending::Agent(Ok(_)) if cancelled => (Close::Cancelled, None),
            Ending::Agent(Err(e)) if cancelled && is_cancellation(&*e) => (Close::Cancelled, None),
            Ending::Agent(Err(e)) => {
                let event = agent_error(&*e, usage.clone());
                return vec![self.verified(event, usage)];
            }
            Ending::Agent(Ok(RunOutcome { kind, result })) => match kind {
                OutcomeKind::Success => (Close::Success, result),
                OutcomeKind::Interrupt(list) => (Close::Interrupt(list), None),
                OutcomeKind::Cancelled => (Close::Cancelled, None),
                OutcomeKind::Failed(error) => {
                    // The agent's own RUN_ERROR, as given (LIFE-21); usage it
                    // did not state comes from what the run recorded.
                    let mut error = *error;
                    if error.usage.is_none() {
                        error.usage = usage.clone();
                    }
                    let mut st = self.shared.lock();
                    return vec![verified_terminal(
                        &mut st.verifier,
                        Event::RunError(error),
                        usage,
                    )];
                }
            },
        };
        self.close_run(outcome, result, usage)
    }

    fn close_run(
        &mut self,
        outcome: Close,
        result: Option<Value>,
        usage: Option<Vec<TokenUsage>>,
    ) -> Vec<Event> {
        let mut out = Vec::new();
        let shared = Arc::clone(&self.shared);
        let mut st = shared.lock();

        // An invalid interrupt list fails the run before anything is closed:
        // no invocation may be reported suspended on interrupts that are
        // never delivered (D12).
        if let Close::Interrupt(list) = &outcome {
            if let Some(problem) = interrupt_problem(list) {
                let refused = run_error(ErrorCode::ProducerViolation, problem, usage.clone());
                out.push(verified_terminal(&mut st.verifier, refused, usage));
                return out;
            }
        }

        // Close what the run opened, innermost first (LIFE-12, LIFE-51). An
        // invocation still open ends the way the run does (D12).
        let mut closers = Vec::new();
        st.verifier.closers(&mut closers);
        for closer in closers {
            let closer = outcome.invocation_closer(closer);
            if let Err(violation) = st.verifier.check(&closer) {
                // Cannot close in order: the run did not end cleanly (LIFE-13).
                let refused = run_error(
                    ErrorCode::ProducerViolation,
                    &format!(
                        "the run's open items could not be closed: {}",
                        violation.message
                    ),
                    usage.clone(),
                );
                // A RUN_ERROR closes nothing (D12): the closers already
                // collected have not reached the wire and are dropped.
                out.clear();
                out.push(verified_terminal(&mut st.verifier, refused, usage));
                return out;
            }
            out.push(closer);
        }

        let wire_outcome = match outcome {
            Close::Success => {
                let pending = st.verifier.unanswered_tool_calls();
                if pending.is_empty() {
                    RunFinishedOutcome::success()
                } else {
                    // TOOL-29..TOOL-31: exactly the unanswered calls, in order.
                    RunFinishedOutcome::success_with_pending(pending)
                }
            }
            Close::Cancelled => RunFinishedOutcome::cancelled(),
            Close::Interrupt(list) => RunFinishedOutcome::interrupt(list),
        };

        let mut finished = RunFinished::new(self.thread_id(), self.run_id());
        finished.outcome = Some(wire_outcome);
        finished.result = result;
        finished.usage = usage.clone();
        let finished = Event::RunFinished(finished);
        match st.verifier.check(&finished) {
            Ok(()) => out.push(finished),
            Err(violation) => {
                let refused = run_error(
                    ErrorCode::ProducerViolation,
                    &violation.message,
                    usage.clone(),
                );
                // The run fails instead: no invocation may be reported
                // closed by a RUN_FINISHED that is never sent (D12).
                out.clear();
                out.push(verified_terminal(&mut st.verifier, refused, usage));
            }
        }
        out
    }

    fn thread_id(&self) -> ThreadId {
        self.ids.0.clone()
    }

    fn run_id(&self) -> RunId {
        self.ids.1.clone()
    }

    /// Applies the legacy-peer downgrade, if any, to an outgoing event
    /// (VER-03). The era shims work on the JSON form; the result is read
    /// back into an event. A downgraded shape this SDK's types cannot hold
    /// is sent as it was.
    fn output(&mut self, event: Event) -> Option<Event> {
        let Some(shims) = &self.downgrade else {
            return Some(event);
        };
        let mut value = match serde_json::to_value(&event) {
            Ok(value) => value,
            Err(_) => return Some(event),
        };
        let keep = shims.downgrade_typed_event(&mut value, &mut self.reporter);
        self.reporter.end_event(None);
        if self.sink.panicked() {
            // Fails the run at the next decision point; an event already
            // verified is still delivered, and a terminal already decided
            // stays the one terminal.
            let failure = sink_panicked();
            self.shared.lock().poison(failure.code, &failure.message);
        }
        if !keep {
            return None;
        }
        match serde_json::from_value(value) {
            Ok(downgraded) => Some(downgraded),
            Err(_) => {
                // Not expected: the typed downgrade keeps to shapes `Event`
                // holds. Say so rather than send the 1.0 shape silently.
                self.reporter.report(|| {
                    Diagnostic::new(
                        DiagnosticCode::LossyDowngrade,
                        Scope::Event,
                        "",
                        "downgraded shape not expressible; event sent in the 1.0 shape; \
                         upgrade the peer to 1.0",
                    )
                    .with_event_type(event.event_type())
                });
                self.reporter.end_event(None);
                Some(event)
            }
        }
    }
}

/// The non-error closings.
enum Close {
    Success,
    Interrupt(Vec<Interrupt>),
    Cancelled,
}

impl Close {
    /// The closer for a still-active invocation under this closing (D12;
    /// `lifecycle.mdx`, `subagents.mdx`): a cancelled run fails it with
    /// `SUBAGENT_ERROR`, since an absent outcome would mean success; an
    /// interrupted run suspends it, naming the run's interrupts it raised
    /// itself (SUB-35); a successful run finishes it. Other closers pass.
    fn invocation_closer(&self, closer: Event) -> Event {
        let Event::SubagentFinished(finished) = closer else {
            return closer;
        };
        match self {
            Close::Success => Event::SubagentFinished(finished),
            Close::Cancelled => {
                SubagentError::new(finished.subagent_run_id, "cancelled with the run").into()
            }
            Close::Interrupt(list) => {
                let own: Vec<InterruptId> = list
                    .iter()
                    .filter(|i| i.subagent_run_id.as_ref() == Some(&finished.subagent_run_id))
                    .map(|i| i.id.clone())
                    .collect();
                let outcome = if own.is_empty() {
                    SubagentFinishedOutcome::Suspended {
                        interrupt_ids: None,
                    }
                } else {
                    SubagentFinishedOutcome::suspended(own)
                };
                finished.with_outcome(outcome).into()
            }
        }
    }
}

/// Why an interrupt list cannot be sent (INTR-01, INTR-04), if it cannot.
fn interrupt_problem(list: &[Interrupt]) -> Option<&'static str> {
    if list.is_empty() {
        return Some("an interrupt outcome needs at least one interrupt");
    }
    let mut seen = HashSet::with_capacity(list.len());
    if list.iter().any(|interrupt| !seen.insert(&interrupt.id)) {
        return Some("interrupt ids must be unique within the run");
    }
    None
}

fn sink_panicked() -> Failure {
    Failure {
        code: ErrorCode::AgentPanicked,
        message: "the diagnostic sink panicked".to_owned(),
    }
}

/// A terminal event that has passed the producer verifier (ARCH-04). A
/// refused one becomes a `producer.violation` RUN_ERROR naming the rule; if
/// the recorded usage is what the verifier refused, that error drops it
/// rather than repeat it.
fn verified_terminal(
    verifier: &mut ProducerVerifier,
    event: Event,
    usage: Option<Vec<TokenUsage>>,
) -> Event {
    let Err(violation) = verifier.check(&event) else {
        return event;
    };
    let replacement = run_error(ErrorCode::ProducerViolation, &violation.message, usage);
    if verifier.check(&replacement).is_ok() {
        return replacement;
    }
    run_error(ErrorCode::ProducerViolation, &violation.message, None)
}

fn run_error(code: ErrorCode, message: &str, usage: Option<Vec<TokenUsage>>) -> Event {
    let mut error = RunError::new(message);
    error.code = Some(code.as_str().to_owned());
    error.usage = usage;
    Event::RunError(error)
}

/// An earlier run restated as snapshots (LIFE-43, LIFE-46, RUNIN-06).
fn replay_events(thread_id: &ThreadId, run: ReplayedRun, declare: bool) -> [Event; 4] {
    let mut started = RunStarted::new(thread_id.clone(), run.run_id.clone());
    if declare {
        started.protocol_version = Some(crate::PROTOCOL_VERSION.to_owned());
    }
    let mut finished = RunFinished::new(thread_id.clone(), run.run_id);
    finished.outcome = run.outcome;
    [
        started.into(),
        MessagesSnapshot::new(run.messages).into(),
        StateSnapshot::new(run.state).into(),
        finished.into(),
    ]
}

/// Typed state from the input (`{}` when absent, RUNIN-24).
fn decode_state<S: serde::de::DeserializeOwned>(state: Option<&Value>) -> Result<S> {
    let decoded = match state {
        Some(value) => S::deserialize(value),
        None => serde_json::from_value(Value::Object(serde_json::Map::new())),
    };
    decoded.map_err(|e| {
        Error::new(
            ErrorCode::InputRejected,
            "the input state does not fit the agent's state type",
        )
        .with_pointer("/state")
        .with_http_status(400)
        .with_source(e)
    })
}

/// Whether an agent's error is, or wraps (through `source`), the
/// cancellation its sends reported.
fn is_cancellation(error: &(dyn std::error::Error + Send + Sync + 'static)) -> bool {
    let error: &(dyn std::error::Error + 'static) = error;
    std::iter::successors(Some(error), |e| e.source()).any(|e| {
        e.downcast_ref::<Error>()
            .is_some_and(|e| e.code() == ErrorCode::Cancelled)
    })
}

/// `RUN_ERROR` for an agent's `Err`: the code and message of a public
/// [`Error`], otherwise a generic `agent.failed` that leaks nothing (LIFE-21).
fn agent_error(
    error: &(dyn std::error::Error + Send + Sync + 'static),
    usage: Option<Vec<TokenUsage>>,
) -> Event {
    match error.downcast_ref::<Error>() {
        Some(e) if e.is_public() => run_error(e.code(), e.message(), usage),
        _ => run_error(ErrorCode::AgentFailed, "the agent failed", usage),
    }
}
