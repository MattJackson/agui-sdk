//! Emitting events from agent code.
//!
//! [`Emitter`] sends events into the run's bounded queue. Every send is
//! checked by the producer verifier at the call site, so a violation is an
//! `Err` where the agent made it, not a broken stream later. Sends are
//! `async`: when the queue is full the agent waits (backpressure) instead of
//! buffering without bound.
//!
//! The item helpers ([`TextStream`], [`ToolCallStream`], [`ReasoningSpan`],
//! [`StepSpan`], [`SubagentScope`]) open an item, carry its id and owner, and
//! close it; anything an agent leaves open is closed by the runner before the
//! terminal event (STRM-03, LIFE-12).
//!
//! Cost per send: one verifier check (O(1)), one queue slot; no boxing.

use std::collections::{HashMap, HashSet, VecDeque};
use std::future::poll_fn;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::task::{Poll, Waker};

use serde_json::Value;

use crate::activity::{ActivityDelta, ActivitySnapshot};
use crate::content::Content;
use crate::error::{Error, ErrorCode, Result};
use crate::event::Event;
use crate::ids::{MessageId, SubagentRunId, ToolCallId};
use crate::json::JsonObject;
use crate::lifecycle::{StepFinished, StepStarted};
use crate::passthrough::{Custom, Raw};
use crate::patch::PatchOperation;
use crate::pipeline::verify::{Holder, ProducerVerifier};
use crate::reasoning::{
    EncryptedValueSubtype, ReasoningEncryptedValue, ReasoningEnd, ReasoningMessageContent,
    ReasoningMessageEnd, ReasoningMessageStart, ReasoningRole, ReasoningStart,
};
use crate::subagent::{SubagentError, SubagentFinished, SubagentFinishedOutcome, SubagentStarted};
use crate::text::{TextMessageContent, TextMessageEnd, TextMessageRole, TextMessageStart};
use crate::tool::{ToolCallArgs, ToolCallEnd, ToolCallResult, ToolCallStart};
use crate::usage::TokenUsage;

use super::state::StatePublisher;
use super::{CancelToken, Transformer};

// ---------------------------------------------------------------------------
// Run-shared machinery: the bounded queue between the agent and the stream,
// the producer verifier, the transformer chain and the state baseline. One
// per run, behind one mutex. Every emitter clone and the runner's stream hold
// an `Arc` to it. The lock is never held across an `.await`, and no user code
// runs under it except transformers, whose panics are caught in place.
// ---------------------------------------------------------------------------

/// Why a run can no longer emit: sticky once set.
#[derive(Debug, Clone)]
pub(super) struct Failure {
    pub(super) code: ErrorCode,
    pub(super) message: String,
}

impl Failure {
    fn to_error(&self) -> Error {
        Error::new(self.code, self.message.clone())
    }
}

/// State shared by one run's emitters and its stream.
pub(super) struct RunShared {
    state: Mutex<RunState>,
    pub(super) cancel: CancelToken,
}

/// Everything behind the run's lock.
pub(super) struct RunState {
    queue: VecDeque<Event>,
    capacity: usize,
    consumer: Option<Waker>,
    turn_held: bool,
    turn_waiters: Vec<Waker>,
    space_waiter: Option<Waker>,
    closed: bool,
    failure: Option<Failure>,
    pub(super) verifier: ProducerVerifier,
    transformers: Vec<Box<dyn Transformer>>,
    staged: Vec<Event>,
    scratch: Vec<Event>,
    pub(super) publisher: StatePublisher,
    frontend_tools: HashSet<String>,
    frontend_calls: HashSet<ToolCallId>,
    subagent_parents: HashMap<SubagentRunId, Option<SubagentRunId>>,
    pub(super) usage: Vec<TokenUsage>,
}

/// What [`RunShared::new`] needs.
pub(super) struct RunSetup {
    pub(super) capacity: usize,
    pub(super) transformers: Vec<Box<dyn Transformer>>,
    pub(super) publisher: StatePublisher,
    pub(super) frontend_tools: HashSet<String>,
    pub(super) verifier: ProducerVerifier,
    pub(super) cancel: CancelToken,
}

impl RunShared {
    pub(super) fn new(setup: RunSetup) -> Arc<Self> {
        let capacity = setup.capacity.max(1);
        Arc::new(Self {
            state: Mutex::new(RunState {
                // Pre-size modestly; a huge configured capacity grows on demand.
                queue: VecDeque::with_capacity(capacity.min(1024)),
                capacity,
                consumer: None,
                turn_held: false,
                turn_waiters: Vec::new(),
                space_waiter: None,
                closed: false,
                failure: None,
                verifier: setup.verifier,
                transformers: setup.transformers,
                staged: Vec::with_capacity(4),
                scratch: Vec::new(),
                publisher: setup.publisher,
                frontend_tools: setup.frontend_tools,
                frontend_calls: HashSet::new(),
                subagent_parents: HashMap::new(),
                usage: Vec::new(),
            }),
            cancel: setup.cancel,
        })
    }

    /// Locks the run state. A poisoned lock is recovered: nothing panics
    /// while holding it except a verifier bug, and the runner then ends the
    /// run with `RUN_ERROR` anyway.
    pub(super) fn lock(&self) -> MutexGuard<'_, RunState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Takes the next queued event, registering the stream's waker if none.
    pub(super) fn poll_pop(&self, waker: &Waker) -> Option<Event> {
        let mut st = self.lock();
        match st.queue.pop_front() {
            Some(event) => {
                if let Some(w) = st.space_waiter.take() {
                    w.wake();
                }
                Some(event)
            }
            None => {
                match &st.consumer {
                    Some(w) if w.will_wake(waker) => {}
                    _ => st.consumer = Some(waker.clone()),
                }
                None
            }
        }
    }

    /// The sticky failure, if any.
    pub(super) fn failure(&self) -> Option<Failure> {
        self.lock().failure.clone()
    }

    /// Stops accepting events: later sends fail with `Disconnected` (or
    /// `Cancelled`), and every waiting sender is woken to observe it.
    pub(super) fn close(&self) {
        let mut st = self.lock();
        st.closed = true;
        st.wake_all();
    }

    async fn acquire_turn(&self) -> Result<Turn<'_>> {
        poll_fn(|cx| {
            let mut st = self.lock();
            if let Err(e) = st.check_open(&self.cancel) {
                return Poll::Ready(Err(e));
            }
            if !st.turn_held {
                st.turn_held = true;
                return Poll::Ready(Ok(()));
            }
            if !st.turn_waiters.iter().any(|w| w.will_wake(cx.waker())) {
                st.turn_waiters.push(cx.waker().clone());
            }
            Poll::Pending
        })
        .await?;
        Ok(Turn { shared: self })
    }

    async fn wait_space(&self, count: usize) -> Result<()> {
        poll_fn(|cx| {
            let mut st = self.lock();
            if let Err(e) = st.check_open(&self.cancel) {
                return Poll::Ready(Err(e));
            }
            if st.capacity - st.queue.len() >= count {
                return Poll::Ready(Ok(()));
            }
            match &st.space_waiter {
                Some(w) if w.will_wake(cx.waker()) => {}
                _ => st.space_waiter = Some(cx.waker().clone()),
            }
            Poll::Pending
        })
        .await
    }

    /// Sends what `prepare` produces. `prepare` runs under the send turn and
    /// the lock, so it sees the baseline every earlier event left behind.
    /// `Staging::Transform` runs the transformer chain; `Staging::Direct`
    /// skips it (runner-made closers, built from post-transform state).
    async fn send_with<F>(&self, owner: Option<&SubagentRunId>, prepare: F) -> Result<()>
    where
        F: FnOnce(&mut RunState) -> Result<Staging>,
    {
        let _turn = self.acquire_turn().await?;
        let mut prepare = Some(prepare);
        let count = loop {
            let step = {
                let mut st = self.lock();
                st.check_open(&self.cancel)?;
                if st.staged.is_empty() {
                    let Some(prepare) = prepare.take() else {
                        return Ok(());
                    };
                    Ok(match prepare(&mut st)? {
                        Staging::Nothing => 0,
                        Staging::Transform(event) => st.stage(event, owner)?,
                        Staging::Direct => st.check_staged()?,
                    })
                } else {
                    // A batch left staged by a send that was dropped while it
                    // waited for queue space. The transformers have already
                    // seen it (and may have recorded an opened message or a
                    // translated id), so it is committed ahead of this send:
                    // what they saw and what reaches the wire never disagree.
                    Err(st.staged.len())
                }
            };
            match step {
                Ok(count) => break count,
                Err(carried) => {
                    self.wait_space(carried).await?;
                    self.lock().commit(&self.cancel)?;
                }
            }
        };
        if count == 0 {
            return Ok(());
        }
        self.wait_space(count).await?;
        let mut st = self.lock();
        st.commit(&self.cancel)
    }

    pub(super) async fn send(&self, owner: Option<&SubagentRunId>, event: Event) -> Result<()> {
        self.send_with(owner, |_| Ok(Staging::Transform(event)))
            .await
    }
}

/// What a send stages. Unboxed on purpose: boxing the event would cost an
/// allocation per send.
#[allow(clippy::large_enum_variant)]
pub(super) enum Staging {
    /// Nothing to send.
    Nothing,
    /// One agent event, run through the transformers.
    Transform(Event),
    /// `staged` was filled directly.
    Direct,
}

/// Exclusive right to stage and commit, so the transformers, the verifier
/// and the queue all see one order even with several emitter clones.
struct Turn<'a> {
    shared: &'a RunShared,
}

impl Drop for Turn<'_> {
    fn drop(&mut self) {
        let mut st = self.shared.lock();
        st.turn_held = false;
        for w in st.turn_waiters.drain(..) {
            w.wake();
        }
    }
}

impl RunState {
    fn check_open(&self, cancel: &CancelToken) -> Result<()> {
        if let Some(f) = &self.failure {
            return Err(f.to_error());
        }
        if cancel.is_cancelled() {
            return Err(Error::new(ErrorCode::Cancelled, "the run was cancelled"));
        }
        if self.closed {
            return Err(Error::new(
                ErrorCode::Disconnected,
                "the run is over; nothing more can be sent",
            ));
        }
        Ok(())
    }

    fn wake_all(&mut self) {
        if let Some(w) = self.consumer.take() {
            w.wake();
        }
        if let Some(w) = self.space_waiter.take() {
            w.wake();
        }
        for w in self.turn_waiters.drain(..) {
            w.wake();
        }
    }

    pub(super) fn poison(&mut self, code: ErrorCode, message: &str) {
        if self.failure.is_none() {
            self.failure = Some(Failure {
                code,
                message: message.to_owned(),
            });
        }
        self.wake_all();
    }

    fn fail(&mut self, code: ErrorCode, message: &str) -> Error {
        self.poison(code, message);
        Error::new(code, message)
    }

    /// Runs `event` through the transformer chain into `staged`; returns
    /// how many events to commit.
    fn stage(&mut self, mut event: Event, owner: Option<&SubagentRunId>) -> Result<usize> {
        attribute(&mut event, owner);
        if self.transformers.is_empty() {
            self.staged.push(event);
        } else {
            let RunState {
                transformers,
                staged,
                scratch,
                ..
            } = self;
            scratch.clear();
            scratch.push(event);
            let mut panicked = false;
            for transformer in transformers.iter_mut() {
                staged.clear();
                for input in scratch.drain(..) {
                    let out = &mut *staged;
                    let t = &mut **transformer;
                    if catch_unwind(AssertUnwindSafe(|| t.transform(input, out))).is_err() {
                        panicked = true;
                        break;
                    }
                }
                if panicked {
                    break;
                }
                std::mem::swap(scratch, staged);
            }
            if panicked {
                scratch.clear();
                staged.clear();
                return Err(self.fail(ErrorCode::AgentPanicked, "a transformer panicked"));
            }
            std::mem::swap(scratch, staged);
            for e in staged.iter_mut() {
                attribute(e, owner);
            }
        }
        self.check_staged()
    }

    /// Checks what can be checked before waiting for queue space.
    fn check_staged(&mut self) -> Result<usize> {
        if self.staged.iter().any(is_lifecycle) {
            self.staged.clear();
            return Err(self.fail(
                ErrorCode::ProducerViolation,
                "run lifecycle events are emitted by the runner, not the agent",
            ));
        }
        let count = self.staged.len();
        if count > self.capacity {
            self.staged.clear();
            return Err(self.fail(
                ErrorCode::QueueOverflow,
                "one send produced more events than the queue can ever hold",
            ));
        }
        Ok(count)
    }

    /// Verifies and enqueues everything staged, in order. The first refusal
    /// poisons the run; events before it were valid and stay queued.
    fn commit(&mut self, cancel: &CancelToken) -> Result<()> {
        if let Err(e) = self.check_open(cancel) {
            self.staged.clear();
            return Err(e);
        }
        let mut staged = std::mem::take(&mut self.staged);
        let mut result = Ok(());
        for event in staged.drain(..) {
            if result.is_err() {
                continue;
            }
            match self.admit(&event) {
                Ok(()) => {
                    self.observe(&event);
                    self.queue.push_back(event);
                }
                Err(e) => result = Err(e),
            }
        }
        self.staged = staged;
        if let Some(w) = self.consumer.take() {
            w.wake();
        }
        result
    }

    fn admit(&mut self, event: &Event) -> Result<()> {
        if let Event::ToolCallResult(r) = event {
            if self.frontend_calls.contains(&r.tool_call_id) {
                return Err(self.fail(
                    ErrorCode::ProducerViolation,
                    "a frontend tool call is answered by the application, not the agent",
                ));
            }
        }
        match self.verifier.check(event) {
            Ok(()) => Ok(()),
            Err(violation) => Err(self.fail(ErrorCode::ProducerViolation, &violation.message)),
        }
    }

    fn observe(&mut self, event: &Event) {
        match event {
            Event::ToolCallStart(s) if self.frontend_tools.contains(s.tool_call_name.as_str()) => {
                self.frontend_calls.insert(s.tool_call_id.clone());
            }
            Event::SubagentStarted(s) => {
                self.subagent_parents
                    .insert(s.subagent_run_id.clone(), s.parent_subagent_run_id.clone());
            }
            Event::StateSnapshot(_) | Event::StateDelta(_) => self.publisher.observe(event),
            _ => {}
        }
    }

    /// Stages closers for everything `scope` or its descendants left open, at
    /// most `limit` of them, innermost first; returns how many it staged.
    /// Under [`ScopeClose::Fail`] a descendant invocation is closed with
    /// `SUBAGENT_ERROR`, not a success-meaning `SUBAGENT_FINISHED`; under
    /// [`ScopeClose::Suspend`] it is finished as suspended, with no interrupt
    /// ids (D12), never as a success.
    fn stage_scope_closers(
        &mut self,
        scope: &SubagentRunId,
        limit: usize,
        closing: ScopeClose,
    ) -> usize {
        let parents = &self.subagent_parents;
        let mut all = Vec::new();
        self.verifier.closers_where(
            &mut |holder| match holder {
                Holder::Invocation(id) => descends(parents, id, scope),
                Holder::Owned(owner) => {
                    owner.is_some_and(|o| o == scope || descends(parents, o, scope))
                }
            },
            &mut all,
        );
        let mut staged = 0;
        for event in all.into_iter().take(limit) {
            let event = match (closing, event) {
                (ScopeClose::Fail, Event::SubagentFinished(f)) => SubagentError::new(
                    f.subagent_run_id,
                    "a parent invocation failed before this one finished",
                )
                .into(),
                (ScopeClose::Suspend, Event::SubagentFinished(f)) => f
                    .with_outcome(SubagentFinishedOutcome::Suspended {
                        interrupt_ids: None,
                    })
                    .into(),
                (_, event) => event,
            };
            self.staged.push(event);
            staged += 1;
        }
        staged
    }
}

/// Whether `id` was spawned (transitively) by `ancestor`.
fn descends(
    parents: &HashMap<SubagentRunId, Option<SubagentRunId>>,
    id: &SubagentRunId,
    ancestor: &SubagentRunId,
) -> bool {
    let mut current = parents.get(id).and_then(Option::as_ref);
    // Bounded: ids are never reused, so the chain is acyclic.
    for _ in 0..=parents.len() {
        match current {
            Some(parent) if parent == ancestor => return true,
            Some(parent) => current = parents.get(parent).and_then(Option::as_ref),
            None => return false,
        }
    }
    false
}

fn is_lifecycle(event: &Event) -> bool {
    matches!(
        event,
        Event::RunStarted(_) | Event::RunFinished(_) | Event::RunError(_)
    )
}

macro_rules! attributable {
    ($event:expr, $bind:ident => $body:expr, $otherwise:expr) => {
        match $event {
            Event::StepStarted($bind) => $body,
            Event::StepFinished($bind) => $body,
            Event::TextMessageStart($bind) => $body,
            Event::TextMessageContent($bind) => $body,
            Event::TextMessageEnd($bind) => $body,
            Event::TextMessageChunk($bind) => $body,
            Event::ToolCallStart($bind) => $body,
            Event::ToolCallArgs($bind) => $body,
            Event::ToolCallEnd($bind) => $body,
            Event::ToolCallChunk($bind) => $body,
            Event::ToolCallResult($bind) => $body,
            Event::StateSnapshot($bind) => $body,
            Event::StateDelta($bind) => $body,
            Event::ActivitySnapshot($bind) => $body,
            Event::ActivityDelta($bind) => $body,
            Event::ReasoningStart($bind) => $body,
            Event::ReasoningMessageStart($bind) => $body,
            Event::ReasoningMessageContent($bind) => $body,
            Event::ReasoningMessageEnd($bind) => $body,
            Event::ReasoningMessageChunk($bind) => $body,
            Event::ReasoningEnd($bind) => $body,
            Event::ReasoningEncryptedValue($bind) => $body,
            Event::Raw($bind) => $body,
            Event::Custom($bind) => $body,
            // Not attributable: a new `Event` variant must be placed here or above.
            Event::RunStarted(_)
            | Event::RunFinished(_)
            | Event::RunError(_)
            | Event::MessagesSnapshot(_)
            | Event::SubagentStarted(_)
            | Event::SubagentFinished(_)
            | Event::SubagentError(_) => $otherwise,
        }
    };
}

/// The attribution slot of an attributable event.
pub(super) fn owner_slot(event: &mut Event) -> Option<&mut Option<SubagentRunId>> {
    attributable!(event, e => Some(&mut e.subagent_run_id), None)
}

/// Fills in `owner` when the event carries no attribution of its own.
fn attribute(event: &mut Event, owner: Option<&SubagentRunId>) {
    if let Some(owner) = owner {
        if let Some(slot) = owner_slot(event) {
            if slot.is_none() {
                *slot = Some(owner.clone());
            }
        }
    }
}

/// Sends events for one run, optionally attributed to a subagent invocation.
/// Clones share the run: send from spawned subtasks freely.
///
/// Every send is checked by the producer verifier before it is queued, so a
/// misordered event is an `Err` at the call site, never a bad stream.
///
/// ```
/// use agui_sdk::server::{Agent, BoxError, RunContext, RunOutcome};
/// use agui_sdk::text::TextMessageRole;
///
/// struct Weather;
///
/// impl Agent for Weather {
///     type State = serde_json::Value;
///
///     async fn run(&self, ctx: RunContext) -> Result<RunOutcome, BoxError> {
///         let emitter = ctx.emitter();
///
///         // A tool call the agent runs itself, with streamed arguments...
///         let call = emitter.start_tool_call("get_weather", None).await?;
///         call.push_args(r#"{"city":"#).await?;
///         call.push_args(r#""Oslo"}"#).await?;
///         let call_id = call.end().await?;
///         emitter.tool_result(call_id, "4 °C, light rain").await?;
///
///         // ...then a streamed answer.
///         let text = emitter.start_text(TextMessageRole::Assistant).await?;
///         text.push("It is 4 °C in Oslo.").await?;
///         text.end().await?;
///         Ok(RunOutcome::success())
///     }
/// }
/// # use agui_sdk::server::{input::{admit, InputPolicy}, CancelToken, Extensions, Runner};
/// # let input = admit(br#"{"threadId":"t-1","runId":"r-1","messages":[]}"#, &InputPolicy::default())?;
/// # let mut stream = Runner::new(Weather).run(input, CancelToken::new(), Extensions::default())?;
/// # let mut n = 0;
/// # futures_executor::block_on(async { while let Some(e) = stream.next().await { n += 1; assert!(!matches!(e, agui_sdk::Event::RunError(_)), "{e:?}"); } });
/// # assert_eq!(n, 10);
/// # Ok::<(), agui_sdk::Error>(())
/// ```
///
/// # Dropping a send
///
/// A send runs the event through the transformers, then waits for queue
/// space. A send future dropped while it waits (a `select!` that lost, a
/// timeout) has therefore already been seen by the transformers: its events
/// stay staged and are committed, verified as usual, ahead of the next send
/// on this run, so a stateful transformer never disagrees with the wire. If
/// no send follows, they are discarded when the run ends. Do not treat a
/// dropped send as "not sent".
#[derive(Clone)]
pub struct Emitter {
    owner: Option<SubagentRunId>,
    run: Arc<RunShared>,
}

impl Emitter {
    pub(super) fn new(run: Arc<RunShared>) -> Self {
        Self { owner: None, run }
    }

    fn scoped(&self, owner: SubagentRunId) -> Self {
        Self {
            owner: Some(owner),
            run: Arc::clone(&self.run),
        }
    }

    async fn send(&self, event: Event) -> Result<()> {
        self.run.send(self.owner.as_ref(), event).await
    }

    /// Publishes state through the run's baseline (see `RunContext`).
    pub(super) async fn send_state(&self, next: Value, resync: bool) -> Result<()> {
        self.run
            .send_with(self.owner.as_ref(), move |st| {
                let event = if resync {
                    Some(st.publisher.snapshot_event(next))
                } else {
                    st.publisher.prepare_owned(next)
                };
                Ok(event.map_or(Staging::Nothing, Staging::Transform))
            })
            .await
    }

    /// Records model usage for the run's terminal event (`RUN_FINISHED` or
    /// `RUN_ERROR`); entries for the same provider and model are summed
    /// (LIFE-08, LIFE-21, LIFE-34). Held in run state shared by every clone,
    /// so it survives the agent returning `Err`. Usage from a subagent scope
    /// counts toward the run; subagent events carry none (LIFE-34). A count is
    /// reported only when every record for the provider and model reported it.
    pub fn record_usage(&self, usage: TokenUsage) {
        merge_usage(&mut self.run.lock().usage, usage);
    }

    /// Sends one event. Lifecycle events (`RUN_*`) are refused: the runner
    /// owns them. Attribution is added from this emitter's owner when the
    /// event has none.
    ///
    /// # Errors
    ///
    /// - [`ErrorCode::ProducerViolation`]: the event (or one a transformer
    ///   made from it) breaks a producer rule.
    /// - [`ErrorCode::QueueOverflow`]: the transformers expanded this one
    ///   send into more events than the queue can hold.
    /// - [`ErrorCode::AgentPanicked`]: a transformer or the diagnostic sink
    ///   panicked.
    ///
    /// Any of these ends the run: later sends fail with the same error and
    /// the run closes with `RUN_ERROR`. When a transformer expanded the
    /// event and one of the expanded events fails verification, the ones
    /// before it are still sent; a lifecycle event among them, or an
    /// expansion beyond the queue, sends none.
    ///
    /// [`ErrorCode::Cancelled`] or [`ErrorCode::Disconnected`]: the run was
    /// cancelled or its stream is gone; nothing more is sent.
    pub async fn emit(&self, event: impl Into<Event>) -> Result<()> {
        self.send(event.into()).await
    }

    /// Sends a complete assistant text message; returns its id.
    ///
    /// # Errors
    ///
    /// As [`emit`](Self::emit).
    pub async fn say(&self, text: impl Into<String>) -> Result<MessageId> {
        let text = text.into();
        let stream = self.start_text(TextMessageRole::Assistant).await?;
        if !text.is_empty() {
            stream.push(text).await?;
        }
        let id = stream.id.clone();
        stream.end().await?;
        Ok(id)
    }

    /// Opens a text message with a fresh id.
    ///
    /// # Errors
    ///
    /// As [`emit`](Self::emit).
    pub async fn start_text(&self, role: TextMessageRole) -> Result<TextStream> {
        let id = MessageId::random();
        let mut start = TextMessageStart::new(id.clone());
        start.role = Some(role);
        self.send(start.into()).await?;
        Ok(TextStream {
            id,
            emitter: self.clone(),
        })
    }

    /// Opens a tool call with a fresh id, attached to `parent_message_id`.
    ///
    /// # Errors
    ///
    /// As [`emit`](Self::emit).
    pub async fn start_tool_call(
        &self,
        name: impl Into<String>,
        parent_message_id: Option<MessageId>,
    ) -> Result<ToolCallStream> {
        let id = ToolCallId::random();
        let mut start = ToolCallStart::new(id.clone(), name);
        start.parent_message_id = parent_message_id;
        self.send(start.into()).await?;
        Ok(ToolCallStream {
            id,
            emitter: self.clone(),
        })
    }

    /// Answers a call the agent executed (never a frontend call, TOOL-24).
    /// Content must be a string or parts (TOOL-13). Returns the tool message id.
    ///
    /// # Errors
    ///
    /// As [`emit`](Self::emit).
    pub async fn tool_result(
        &self,
        tool_call_id: ToolCallId,
        content: impl Into<Content>,
    ) -> Result<MessageId> {
        let id = MessageId::random();
        let result = ToolCallResult::new(id.clone(), tool_call_id, content.into());
        self.send(result.into()).await?;
        Ok(id)
    }

    /// Opens a reasoning span.
    ///
    /// # Errors
    ///
    /// As [`emit`](Self::emit).
    pub async fn start_reasoning(&self) -> Result<ReasoningSpan> {
        let id = MessageId::random();
        self.send(ReasoningStart::new(id.clone()).into()).await?;
        Ok(ReasoningSpan {
            id,
            emitter: self.clone(),
        })
    }

    /// Opens a step.
    ///
    /// # Errors
    ///
    /// As [`emit`](Self::emit).
    pub async fn start_step(&self, name: impl Into<String>) -> Result<StepSpan> {
        let name = name.into();
        self.send(StepStarted::new(name.clone()).into()).await?;
        Ok(StepSpan {
            name,
            emitter: self.clone(),
        })
    }

    /// Announces a subagent invocation and returns its scope (SUB-07, SUB-13).
    ///
    /// # Errors
    ///
    /// As [`emit`](Self::emit).
    pub async fn start_subagent(&self, name: impl Into<String>) -> Result<SubagentScope> {
        let id = SubagentRunId::random();
        let mut started = SubagentStarted::new(id.clone(), name);
        started.parent_subagent_run_id = self.owner.clone();
        self.send(started.into()).await?;
        Ok(SubagentScope {
            emitter: self.scoped(id.clone()),
            id,
        })
    }

    /// Creates or replaces an activity message.
    ///
    /// # Errors
    ///
    /// As [`emit`](Self::emit).
    pub async fn activity_snapshot(
        &self,
        message_id: MessageId,
        activity_type: impl Into<String>,
        content: JsonObject,
    ) -> Result<()> {
        self.send(ActivitySnapshot::new(message_id, activity_type, content).into())
            .await
    }

    /// Patches an activity message created by a snapshot (ACT-11). Repeats
    /// the current type unless retyping (ACT-10).
    ///
    /// # Errors
    ///
    /// As [`emit`](Self::emit).
    pub async fn activity_delta(
        &self,
        message_id: MessageId,
        activity_type: impl Into<String>,
        patch: Vec<PatchOperation>,
    ) -> Result<()> {
        self.send(ActivityDelta::new(message_id, activity_type, patch).into())
            .await
    }

    /// Sends a `CUSTOM` event. Prefix invented names (PASS-07).
    ///
    /// # Errors
    ///
    /// As [`emit`](Self::emit).
    pub async fn custom(&self, name: impl Into<String>, value: Value) -> Result<()> {
        self.send(Custom::new(name, value).into()).await
    }

    /// Sends a `RAW` event alongside, never instead of, standard events (PASS-04).
    ///
    /// # Errors
    ///
    /// As [`emit`](Self::emit).
    pub async fn raw(&self, event: Value, source: Option<String>) -> Result<()> {
        let mut raw = Raw::new(event);
        raw.source = source;
        self.send(raw.into()).await
    }

    /// Attaches an opaque encrypted value to a message or tool call (REAS-19).
    ///
    /// # Errors
    ///
    /// As [`emit`](Self::emit).
    pub async fn encrypted_value(
        &self,
        subtype: EncryptedValueSubtype,
        entity_id: impl Into<String>,
        value: impl Into<String>,
    ) -> Result<()> {
        self.send(ReasoningEncryptedValue::new(subtype, entity_id, value).into())
            .await
    }
}

impl std::fmt::Debug for Emitter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Emitter")
            .field("owner", &self.owner)
            .finish_non_exhaustive()
    }
}

/// An open text message.
#[derive(Debug)]
#[must_use = "the item stays open until it is ended (`end`, `finish` or `fail`) or the run's closers close it"]
pub struct TextStream {
    id: MessageId,
    emitter: Emitter,
}

impl TextStream {
    /// The message id.
    #[must_use]
    pub fn id(&self) -> &MessageId {
        &self.id
    }

    /// Appends text.
    ///
    /// # Errors
    ///
    /// As [`Emitter::emit`].
    pub async fn push(&self, delta: impl Into<String>) -> Result<()> {
        self.emitter
            .send(TextMessageContent::new(self.id.clone(), delta).into())
            .await
    }

    /// Closes the message.
    ///
    /// # Errors
    ///
    /// As [`Emitter::emit`].
    pub async fn end(self) -> Result<()> {
        self.emitter.send(TextMessageEnd::new(self.id).into()).await
    }
}

/// An open tool call.
#[derive(Debug)]
#[must_use = "the item stays open until it is ended (`end`, `finish` or `fail`) or the run's closers close it"]
pub struct ToolCallStream {
    id: ToolCallId,
    emitter: Emitter,
}

impl ToolCallStream {
    /// The call id.
    #[must_use]
    pub fn id(&self) -> &ToolCallId {
        &self.id
    }

    /// Appends argument text.
    ///
    /// # Errors
    ///
    /// As [`Emitter::emit`].
    pub async fn push_args(&self, delta: impl Into<String>) -> Result<()> {
        self.emitter
            .send(ToolCallArgs::new(self.id.clone(), delta).into())
            .await
    }

    /// Closes the call (arguments complete). A frontend call is left
    /// unanswered and reported in `pendingToolCallIds` (TOOL-24, TOOL-25).
    ///
    /// # Errors
    ///
    /// As [`Emitter::emit`].
    pub async fn end(self) -> Result<ToolCallId> {
        self.emitter
            .send(ToolCallEnd::new(self.id.clone()).into())
            .await?;
        Ok(self.id)
    }
}

/// An open reasoning span.
#[derive(Debug)]
#[must_use = "the item stays open until it is ended (`end`, `finish` or `fail`) or the run's closers close it"]
pub struct ReasoningSpan {
    id: MessageId,
    emitter: Emitter,
}

impl ReasoningSpan {
    /// Opens a reasoning message inside the span.
    ///
    /// # Errors
    ///
    /// As [`Emitter::emit`].
    pub async fn start_message(&self) -> Result<ReasoningStream> {
        let id = MessageId::random();
        self.emitter
            .send(ReasoningMessageStart::new(id.clone(), ReasoningRole::Reasoning).into())
            .await?;
        Ok(ReasoningStream {
            id,
            emitter: self.emitter.clone(),
        })
    }

    /// Closes the span.
    ///
    /// # Errors
    ///
    /// As [`Emitter::emit`].
    pub async fn end(self) -> Result<()> {
        self.emitter.send(ReasoningEnd::new(self.id).into()).await
    }
}

/// An open reasoning message.
#[derive(Debug)]
#[must_use = "the item stays open until it is ended (`end`, `finish` or `fail`) or the run's closers close it"]
pub struct ReasoningStream {
    id: MessageId,
    emitter: Emitter,
}

impl ReasoningStream {
    /// Appends reasoning text.
    ///
    /// # Errors
    ///
    /// As [`Emitter::emit`].
    pub async fn push(&self, delta: impl Into<String>) -> Result<()> {
        self.emitter
            .send(ReasoningMessageContent::new(self.id.clone(), delta).into())
            .await
    }

    /// Closes the message.
    ///
    /// # Errors
    ///
    /// As [`Emitter::emit`].
    pub async fn end(self) -> Result<()> {
        self.emitter
            .send(ReasoningMessageEnd::new(self.id).into())
            .await
    }
}

/// An open step.
#[derive(Debug)]
#[must_use = "the item stays open until it is ended (`end`, `finish` or `fail`) or the run's closers close it"]
pub struct StepSpan {
    name: String,
    emitter: Emitter,
}

impl StepSpan {
    /// Closes the step with the same name and owner (SUB-22).
    ///
    /// # Errors
    ///
    /// As [`Emitter::emit`].
    pub async fn finish(self) -> Result<()> {
        self.emitter.send(StepFinished::new(self.name).into()).await
    }
}

/// Why a scope's still-open items are being closed.
#[derive(Clone, Copy)]
enum ScopeClose {
    Finish,
    /// The scope itself is finished as suspended (D12).
    Suspend,
    Fail,
}

/// An announced subagent invocation. Its [`emitter`](Self::emitter) attributes
/// every event to the invocation (SUB-01).
#[derive(Debug)]
#[must_use = "the item stays open until it is ended (`end`, `finish` or `fail`) or the run's closers close it"]
pub struct SubagentScope {
    id: SubagentRunId,
    emitter: Emitter,
}

impl SubagentScope {
    /// Closes whatever this invocation and its descendants left open.
    async fn close_open(&self, closing: ScopeClose) -> Result<()> {
        let scope = &self.id;
        // In chunks the queue can hold: the closers are the SDK's, so a scope
        // with more open items than the queue is backpressure, never a
        // failure.
        loop {
            let staged = AtomicUsize::new(0);
            let capacity = AtomicUsize::new(0);
            self.emitter
                .run
                .send_with(None, |st| {
                    capacity.store(st.capacity, Ordering::Relaxed);
                    let n = st.stage_scope_closers(scope, st.capacity, closing);
                    staged.store(n, Ordering::Relaxed);
                    Ok(Staging::Direct)
                })
                .await?;
            if staged.load(Ordering::Relaxed) < capacity.load(Ordering::Relaxed).max(1) {
                return Ok(());
            }
        }
    }

    /// The invocation id.
    #[must_use]
    pub fn id(&self) -> &SubagentRunId {
        &self.id
    }

    /// An emitter attributing events to this invocation.
    #[must_use]
    pub fn emitter(&self) -> &Emitter {
        &self.emitter
    }

    /// Announces a nested invocation spawned by this one (SUB-27).
    ///
    /// # Errors
    ///
    /// As [`Emitter::emit`].
    pub async fn start_subagent(&self, name: impl Into<String>) -> Result<SubagentScope> {
        self.emitter.start_subagent(name).await
    }

    /// Ends the invocation. Its open items are closed first (STRM-26); with a
    /// suspended outcome its still-active descendants are suspended too.
    ///
    /// # Errors
    ///
    /// As [`Emitter::emit`].
    pub async fn finish(
        self,
        outcome: Option<SubagentFinishedOutcome>,
        result: Option<Value>,
    ) -> Result<()> {
        let closing = if matches!(outcome, Some(SubagentFinishedOutcome::Suspended { .. })) {
            ScopeClose::Suspend
        } else {
            ScopeClose::Finish
        };
        self.close_open(closing).await?;
        let mut finished = SubagentFinished::new(self.id);
        finished.outcome = outcome;
        finished.result = result.and_then(crate::json::present);
        self.emitter.send(finished.into()).await
    }

    /// Ends the invocation in failure; the run continues (SUB-14).
    ///
    /// # Errors
    ///
    /// As [`Emitter::emit`].
    pub async fn fail(self, message: impl Into<String>) -> Result<()> {
        self.close_open(ScopeClose::Fail).await?;
        self.emitter
            .send(SubagentError::new(self.id, message).into())
            .await
    }
}

/// Adds `usage` into the entry for the same provider and model. The total is
/// always recomputed from the two totals when both are known, never copied
/// from a provider that may count differently (LIFE-29..LIFE-31); counts
/// nobody reported stay absent rather than becoming zero (LIFE-32): a merged
/// count is known only if every merged record reported it.
fn merge_usage(all: &mut Vec<TokenUsage>, mut usage: TokenUsage) {
    fn add(a: Option<u64>, b: Option<u64>) -> Option<u64> {
        Some(a?.saturating_add(b?))
    }
    // Each record is one call: its total is its own parts when both are known.
    if let (Some(input), Some(output)) = (usage.input_tokens, usage.output_tokens) {
        usage.total_tokens = Some(input.saturating_add(output));
    }
    let index = match all
        .iter()
        .position(|u| u.provider == usage.provider && u.model == usage.model)
    {
        Some(index) => {
            let entry = &mut all[index];
            entry.input_tokens = add(entry.input_tokens, usage.input_tokens);
            entry.output_tokens = add(entry.output_tokens, usage.output_tokens);
            entry.reasoning_tokens = add(entry.reasoning_tokens, usage.reasoning_tokens);
            entry.cached_input_tokens = add(entry.cached_input_tokens, usage.cached_input_tokens);
            entry.cache_write_input_tokens = add(
                entry.cache_write_input_tokens,
                usage.cache_write_input_tokens,
            );
            entry.total_tokens = add(entry.total_tokens, usage.total_tokens);
            index
        }
        None => {
            all.push(usage);
            all.len() - 1
        }
    };
    let entry = &mut all[index];
    if let (Some(input), Some(output)) = (entry.input_tokens, entry.output_tokens) {
        entry.total_tokens = Some(input.saturating_add(output));
    }
}
