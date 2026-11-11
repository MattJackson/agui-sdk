use serde_json::Value;

use crate::diagnostics::Diagnostic;
use crate::error::Error;
use crate::ids::{RunId, ThreadId};
use crate::message::Message;
use crate::pipeline::reduce::PendingToolCall;

use super::{RunReport, RunUpdate};

/// Which run a callback is about. One observer serves every concurrent run
/// of an [`AgentClient`](super::AgentClient); this tells them apart.
#[derive(Debug, Clone, Copy)]
pub struct RunInfo<'a> {
    thread_id: &'a ThreadId,
    run_id: &'a RunId,
}

impl<'a> RunInfo<'a> {
    /// A run identity, for calling an [`Observer`] directly in a test.
    #[must_use]
    pub fn new(thread_id: &'a ThreadId, run_id: &'a RunId) -> Self {
        RunInfo { thread_id, run_id }
    }

    /// The run's thread.
    #[must_use]
    pub fn thread_id(&self) -> &'a ThreadId {
        self.thread_id
    }

    /// The requested run id.
    #[must_use]
    pub fn run_id(&self) -> &'a RunId {
        self.run_id
    }
}

/// A borrowed view of the run's reduced state at the moment of a callback.
/// Nothing is copied to build it.
#[derive(Debug, Clone, Copy)]
pub struct RunView<'a> {
    run: RunInfo<'a>,
    messages: &'a [Message],
    state: &'a Value,
    pending: &'a [PendingToolCall],
}

impl<'a> RunView<'a> {
    /// A view over borrowed history, state and pending calls, for calling an
    /// [`Observer`] directly in a test.
    #[must_use]
    pub fn new(
        run: RunInfo<'a>,
        messages: &'a [Message],
        state: &'a Value,
        pending: &'a [PendingToolCall],
    ) -> Self {
        RunView {
            run,
            messages,
            state,
            pending,
        }
    }

    /// The run's identity.
    #[must_use]
    pub fn run(&self) -> RunInfo<'a> {
        self.run
    }

    /// The run's thread.
    #[must_use]
    pub fn thread_id(&self) -> &'a ThreadId {
        self.run.thread_id
    }

    /// The requested run id.
    #[must_use]
    pub fn run_id(&self) -> &'a RunId {
        self.run.run_id
    }

    /// History so far.
    #[must_use]
    pub fn messages(&self) -> &'a [Message] {
        self.messages
    }

    /// State so far.
    #[must_use]
    pub fn state(&self) -> &'a Value {
        self.state
    }

    /// Frontend calls ended and not yet answered (TOOL-32).
    #[must_use]
    pub fn pending_tool_calls(&self) -> &'a [PendingToolCall] {
        self.pending
    }
}

/// Read-only run notifications. Synchronous and object-safe; every method has
/// an empty default, and any method added later will too.
///
/// Per event the order is `on_event` then, for a message or state change, the
/// view already reflects it. A panicking observer is contained: the run fails
/// with `observer.panicked` and the other observers still get `on_finish`,
/// which every observer receives exactly once, even when the handle is dropped
/// mid-run.
///
/// Cost: one virtual call per event per observer; the view borrows.
pub trait Observer: Send + Sync {
    /// A verified event was applied. `view.run()` identifies the run.
    fn on_event(&self, update: &RunUpdate, view: RunView<'_>) {
        let _ = (update, view);
    }

    /// A diagnostic was reported for the run `run`.
    fn on_diagnostic(&self, diagnostic: &Diagnostic, run: RunInfo<'_>) {
        let _ = (diagnostic, run);
    }

    /// The run `run` ended: `Ok` with the report (any status, including a
    /// delivered `RUN_ERROR`), or `Err` for a rejected, truncated or
    /// cancelled stream. A handle dropped before the end reports
    /// `run.cancelled`.
    fn on_finish(&self, result: Result<&RunReport, &Error>, run: RunInfo<'_>) {
        let _ = (result, run);
    }
}
