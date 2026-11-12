use serde_json::Value;

use crate::error::{Error, ErrorCode, Result};
use crate::ids::{RunId, ThreadId};
use crate::interrupt::ResumeEntry;
use crate::message::Message;
use crate::run_input::{Context, RunAgentInput};
use crate::tool::Tool;
use crate::version::PeerVersion;

use super::emit::Emitter;
use super::{CancelToken, Extensions};

/// Everything an agent gets for one run: the admitted input, typed state,
/// the [`Emitter`] (which also records usage), cancellation and the
/// request's [`Extensions`]. Owned by the agent's future; nothing in it
/// borrows from the runner.
pub struct RunContext<S = Value> {
    input: RunAgentInput,
    peer_version: PeerVersion,
    state: S,
    emitter: Emitter,
    cancel: CancelToken,
    extensions: Extensions,
}

impl<S> RunContext<S> {
    pub(super) fn new(
        input: RunAgentInput,
        peer_version: PeerVersion,
        state: S,
        emitter: Emitter,
        cancel: CancelToken,
        extensions: Extensions,
    ) -> Self {
        Self {
            input,
            peer_version,
            state,
            emitter,
            cancel,
            extensions,
        }
    }

    /// The admitted input (unknown members already stripped).
    #[must_use]
    pub fn input(&self) -> &RunAgentInput {
        &self.input
    }

    /// The thread id.
    #[must_use]
    pub fn thread_id(&self) -> &ThreadId {
        &self.input().thread_id
    }

    /// The run id.
    #[must_use]
    pub fn run_id(&self) -> &RunId {
        &self.input().run_id
    }

    /// The complete history the client sent (RUNIN-10).
    #[must_use]
    pub fn messages(&self) -> &[Message] {
        self.input().messages.as_slice()
    }

    /// Frontend tools the client advertised; empty when absent (RUNIN-20).
    /// [`input()`](Self::input) keeps absent and `[]` apart.
    #[must_use]
    pub fn tools(&self) -> &[Tool] {
        self.input.tools()
    }

    /// Grounding entries; see [`helpers::context_prompt`](super::helpers::context_prompt) (RUNIN-22).
    #[must_use]
    pub fn context(&self) -> &[Context] {
        self.input.context()
    }

    /// Resume entries answering the previous run's interrupts.
    #[must_use]
    pub fn resume(&self) -> &[ResumeEntry] {
        self.input.resume()
    }

    /// `forwardedProps`, unaltered (RUNIN-27).
    #[must_use]
    pub fn forwarded_props(&self) -> Option<&Value> {
        self.input.forwarded_props.as_ref()
    }

    /// How the client's declared version relates to ours.
    #[must_use]
    pub fn peer_version(&self) -> &PeerVersion {
        &self.peer_version
    }

    /// Typed state.
    #[must_use]
    pub fn state(&self) -> &S {
        &self.state
    }

    /// Mutable typed state. Changes reach the client on
    /// [`publish_state`](Self::publish_state).
    pub fn state_mut(&mut self) -> &mut S {
        &mut self.state
    }

    /// The event emitter. Cloneable, for spawned subtasks.
    #[must_use]
    pub fn emitter(&self) -> &Emitter {
        &self.emitter
    }

    /// The run's cancellation token.
    ///
    /// The runner does not wait for the agent to wind down: at the first
    /// poll after the token is cancelled it drops the agent's future and
    /// closes the run with the `cancelled` outcome. An `.await` on
    /// [`CancelToken::cancelled`] therefore never resumes inside the agent;
    /// put cleanup that must run (releasing a lease, aborting an upstream
    /// call) in a `Drop` implementation held by the future, and hand
    /// anything that needs `async` work to a task of your own. Spawned
    /// subtasks see the token too, and their sends fail with
    /// [`ErrorCode::Cancelled`](crate::ErrorCode::Cancelled).
    #[must_use]
    pub fn cancellation(&self) -> &CancelToken {
        &self.cancel
    }

    /// Whether the client cancelled or disconnected. See
    /// [`cancellation`](Self::cancellation): once this is true the agent's
    /// future is dropped at the runner's next poll.
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.cancel.is_cancelled()
    }

    /// Request-scoped data from the HTTP layer (principal, tenant, trace).
    #[must_use]
    pub fn extensions(&self) -> &Extensions {
        &self.extensions
    }
}

impl<S: serde::Serialize> RunContext<S> {
    /// Sends the state change since the last publication: a `STATE_DELTA`
    /// against the baseline the client holds (the input's state until the
    /// first snapshot, RUNIN-25, SNAP-07), or a snapshot when a delta would
    /// be larger. No event when nothing changed.
    ///
    /// # Errors
    ///
    /// As [`Emitter::emit`].
    pub async fn publish_state(&mut self) -> Result<()> {
        let next = self.state_value()?;
        self.emitter.send_state(next, false).await
    }

    /// Sends a full `STATE_SNAPSHOT` regardless, e.g. after the agent cannot
    /// rule out divergence (SNAP-04, SNAP-15).
    ///
    /// # Errors
    ///
    /// As [`Emitter::emit`].
    pub async fn resync_state(&mut self) -> Result<()> {
        let next = self.state_value()?;
        self.emitter.send_state(next, true).await
    }

    fn state_value(&self) -> Result<Value> {
        serde_json::to_value(&self.state).map_err(|e| {
            Error::new(ErrorCode::Json, "the state could not be serialized").with_source(e)
        })
    }
}

impl<S> std::fmt::Debug for RunContext<S> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RunContext")
            .field("thread_id", &self.input.thread_id)
            .field("run_id", &self.input.run_id)
            .field("peer_version", &self.peer_version)
            .field("extensions", &self.extensions)
            .finish_non_exhaustive()
    }
}
