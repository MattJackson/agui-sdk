use std::future::Future;
use std::pin::Pin;

use serde::de::DeserializeOwned;
use serde::Serialize;
use serde_json::Value;

use crate::error::BoxError;
use crate::interrupt::Interrupt;
use crate::lifecycle::RunError;

use super::RunContext;

/// Your agent. The runner hands `run` an owned [`RunContext`] once per
/// admitted request and turns whatever comes back into the terminal event.
///
/// `run` returns [`BoxError`] so `?` works on any error type. On `Err` (or
/// a panic) the run ends in `RUN_ERROR`. If the error is an
/// [`agui_sdk::Error`](crate::Error) marked [`public`](crate::Error::public),
/// its code and message are sent; otherwise the client sees a generic
/// `agent.failed` and the detail is dropped: it is neither sent nor
/// reported to the diagnostic sink (LIFE-21). Log it yourself before
/// returning if you need it. For a failure you want to describe yourself,
/// return [`RunOutcome::failed`] instead.
///
/// # Frontend tools
///
/// A call to a tool from [`RunContext::tools`] is the application's to
/// answer: end the call and do not send a result (TOOL-24). Once nothing
/// left to do depends on such a result, return [`RunOutcome::success`]
/// promptly; the runner lists the unanswered calls in `pendingToolCallIds`
/// (TOOL-25, TOOL-27, TOOL-31). The SDK cannot check promptness: that duty is
/// the agent author's.
///
/// # Cancellation
///
/// When the client cancels or disconnects, the runner drops the future
/// returned by [`run`](Self::run) at its next poll and closes the run with
/// the `cancelled` outcome itself (LIFE-11). The agent is not polled again:
/// code after an `.await` does not run, so cleanup belongs in `Drop` (see
/// [`RunContext::cancellation`]).
///
/// Implement with `async fn run` in the impl; the future must be `Send`.
/// Methods added in later minors will have default bodies.
///
/// ```
/// use agui_sdk::server::{Agent, BoxError, RunContext, RunOutcome};
/// use agui_sdk::text::TextMessageRole;
/// use serde::{Deserialize, Serialize};
///
/// /// The shared state, typed: `RunAgentInput.state` deserializes into it.
/// #[derive(Default, Serialize, Deserialize)]
/// struct Counter {
///     count: u64,
/// }
///
/// struct CountingAgent;
///
/// impl Agent for CountingAgent {
///     type State = Counter;
///
///     async fn run(&self, mut ctx: RunContext<Counter>) -> Result<RunOutcome, BoxError> {
///         // Stream a reply token by token.
///         let text = ctx.emitter().start_text(TextMessageRole::Assistant).await?;
///         for token in ["Counting", "..."] {
///             text.push(token).await?;
///         }
///         text.end().await?;
///
///         // Change the state and publish it (a snapshot, then deltas).
///         ctx.state_mut().count += 1;
///         ctx.publish_state().await?;
///         Ok(RunOutcome::success())
///     }
/// }
/// # let _ = agui_sdk::server::Runner::new(CountingAgent);
/// ```
pub trait Agent: Send + Sync + 'static {
    /// The typed view of `RunAgentInput.state`; [`serde_json::Value`] for
    /// untyped. An absent state is read as `{}` (RUNIN-24); a state that does
    /// not deserialize is refused before the stream starts (HTTP 400).
    type State: Serialize + DeserializeOwned + Send + Sync + 'static;

    /// Serves one request.
    fn run(
        &self,
        ctx: RunContext<Self::State>,
    ) -> impl Future<Output = Result<RunOutcome, BoxError>> + Send;
}

mod sealed {
    pub trait Sealed {}
    impl<A: super::Agent> Sealed for A {}
}

/// [`Agent`] behind a vtable, for serving agents chosen at runtime. Every
/// `Agent` gets it for free; it is sealed, so implement `Agent` instead.
pub trait DynAgent: sealed::Sealed + Send + Sync + 'static {
    /// Same as [`Agent::State`].
    type State: Serialize + DeserializeOwned + Send + Sync + 'static;

    /// [`Agent::run`] with the future boxed.
    fn run_boxed(
        &self,
        ctx: RunContext<Self::State>,
    ) -> Pin<Box<dyn Future<Output = Result<RunOutcome, BoxError>> + Send + '_>>;
}

impl<A: Agent> DynAgent for A {
    type State = A::State;

    fn run_boxed(
        &self,
        ctx: RunContext<Self::State>,
    ) -> Pin<Box<dyn Future<Output = Result<RunOutcome, BoxError>> + Send + '_>> {
        Box::pin(self.run(ctx))
    }
}

/// An owned, type-erased agent; it is itself an [`Agent`].
pub type BoxAgent<S = Value> = Box<dyn DynAgent<State = S>>;

impl<S> Agent for BoxAgent<S>
where
    S: Serialize + DeserializeOwned + Send + Sync + 'static,
{
    type State = S;

    async fn run(&self, ctx: RunContext<S>) -> Result<RunOutcome, BoxError> {
        // Dispatch through the trait object itself: `self.run_boxed` would
        // resolve to the blanket impl for the box and recurse forever.
        let agent: &dyn DynAgent<State = S> = &**self;
        agent.run_boxed(ctx).await
    }
}

/// What the agent reports when it returns normally. The runner writes the
/// matching terminal: `RUN_FINISHED` for success, interrupt and cancellation,
/// `RUN_ERROR` for [`failed`](Self::failed).
#[derive(Debug, Clone, PartialEq)]
pub struct RunOutcome {
    pub(super) kind: OutcomeKind,
    pub(super) result: Option<Value>,
}

#[derive(Debug, Clone, PartialEq)]
pub(super) enum OutcomeKind {
    Success,
    Interrupt(Vec<Interrupt>),
    Cancelled,
    Failed(Box<RunError>),
}

impl RunOutcome {
    /// Done. The runner lists any frontend calls still unanswered in
    /// `pendingToolCallIds` (TOOL-25, TOOL-30, TOOL-31). Return it as soon as
    /// nothing remaining depends on a frontend result (TOOL-27).
    #[must_use]
    pub fn success() -> Self {
        Self {
            kind: OutcomeKind::Success,
            result: None,
        }
    }

    /// Paused for the user (INTR-01). The list must be non-empty and its ids
    /// unique (INTR-04), or the runner refuses the outcome.
    #[must_use]
    pub fn interrupt(interrupts: impl IntoIterator<Item = Interrupt>) -> Self {
        Self {
            kind: OutcomeKind::Interrupt(interrupts.into_iter().collect()),
            result: None,
        }
    }

    /// Stopped early, but not a failure (LIFE-10). Open items are closed
    /// before the terminal (LIFE-12); any result is discarded (LIFE-14).
    #[must_use]
    pub fn cancelled() -> Self {
        Self {
            kind: OutcomeKind::Cancelled,
            result: None,
        }
    }

    /// Failed, with a `RUN_ERROR` the agent wrote itself: its code and
    /// message are sent as given (LIFE-21); usage it leaves unset is filled
    /// from what the run recorded with `record_usage`. Use this, not `Err`, when the
    /// client should see the details.
    #[must_use]
    pub fn failed(error: RunError) -> Self {
        Self {
            kind: OutcomeKind::Failed(Box::new(error)),
            result: None,
        }
    }

    /// The run's return value; kept on success only.
    #[must_use]
    pub fn with_result(self, result: Value) -> Self {
        match self.kind {
            OutcomeKind::Success => Self {
                result: Some(result),
                ..self
            },
            // Only a completed run has a return value (LIFE-14).
            _ => self,
        }
    }
}
