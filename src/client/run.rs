use std::collections::HashMap;
use std::future::{Future, IntoFuture};
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use serde::de::DeserializeOwned;
use serde_json::Value;

use crate::cancel::CancelToken;
use crate::diagnostics::Diagnostic;
use crate::error::{Error, ErrorCode, Result};
use crate::event::Event;
use crate::ids::{MessageId, RunId, ThreadId};
use crate::interrupt::Interrupt;
use crate::lifecycle::{RunError, RunFinishedOutcome};
use crate::message::Message;
use crate::pipeline::reduce::{Changes, PendingToolCall};
use crate::usage::TokenUsage;

/// One verified event and what it changed. The event is shared with the
/// reducer (an `Arc`, never deep-cloned); read the reduced history through
/// [`RunHandle::messages`] or an [`Observer`](super::Observer)'s view.
/// Dropping updates promptly lets the reducer own snapshot payloads without
/// any copy.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub struct RunUpdate {
    /// The event, post-pipeline. Chunks arrive expanded.
    pub event: Arc<Event>,
    /// What the reducer changed.
    pub changes: Changes,
}

impl RunUpdate {
    /// An update, for tests of observers and consumers.
    pub fn new(event: impl Into<Arc<Event>>, changes: Changes) -> Self {
        RunUpdate {
            event: event.into(),
            changes,
        }
    }
}

/// How a run that reached a terminal event ended.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum RunStatus {
    /// `RUN_FINISHED`, with its outcome (absent outcome reads as success,
    /// LIFE-05) and result.
    Finished {
        /// The outcome as delivered, after enforcement.
        outcome: Option<RunFinishedOutcome>,
        /// The run's return value.
        result: Option<Value>,
    },
    /// `RUN_ERROR` from the agent: a delivered failure (LIFE-22, LIFE-23).
    /// Also the status after a late `RUN_ERROR` (LIFE-40).
    Errored(RunError),
}

impl RunStatus {
    /// Completed successfully: finished with a success or absent outcome.
    /// False for interrupted, cancelled and errored runs (LIFE-17, LIFE-24, INTR-02).
    #[must_use]
    pub fn is_success(&self) -> bool {
        matches!(
            self,
            RunStatus::Finished {
                outcome: None | Some(RunFinishedOutcome::Success { .. }),
                ..
            }
        )
    }

    /// Finished with the cancelled outcome: neither success nor failure
    /// (LIFE-17, LIFE-18).
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        matches!(
            self,
            RunStatus::Finished {
                outcome: Some(RunFinishedOutcome::Cancelled),
                ..
            }
        )
    }

    /// Finished with the interrupt outcome.
    #[must_use]
    pub fn is_interrupted(&self) -> bool {
        matches!(
            self,
            RunStatus::Finished {
                outcome: Some(RunFinishedOutcome::Interrupt { .. }),
                ..
            }
        )
    }
}

/// Everything a run produced. Owns the history (moved out of the reducer,
/// not copied).
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct RunReport {
    /// Thread of the last run on the stream.
    pub thread_id: ThreadId,
    /// Id of the last run on the stream.
    pub run_id: RunId,
    /// How the last run ended. For a multi-run stream, history and state
    /// accumulate across runs while this describes the last (LIFE-45). In
    /// the partial report of a failed run that reached no terminal event
    /// ([`Error::partial`](crate::Error::partial)) it is
    /// [`RunStatus::Errored`] carrying the client-side error's code and
    /// message, so it never reads as success (TRN-10).
    pub status: RunStatus,
    /// The run the request named. The same as [`run_id`](Self::run_id)
    /// unless the stream carried more runs after it.
    pub requested_run_id: RunId,
    /// How the requested run ended. The same as [`status`](Self::status)
    /// when it was the stream's last run. When a later run followed it on
    /// the same stream (a producer MAY start a new run after `RUN_ERROR`),
    /// this keeps the requested run's own ending, so a requested run that
    /// failed is never reported as a success because a later one succeeded
    /// (LIFE-23). In a partial report it never reads as success either.
    pub requested_status: RunStatus,
    /// Complete history after the run. Reasoning and encrypted values in it
    /// are meant for the agent that produced them: send them back only
    /// through the client that received them, and treat them with the
    /// conversation's confidentiality (IDX-10, REAS-25).
    pub messages: Vec<Message>,
    /// Ids of the messages the run added, in order. Ids rather than an index:
    /// a `MESSAGES_SNAPSHOT` can drop and reorder history (STATE-04..STATE-08).
    pub new_message_ids: Vec<MessageId>,
    /// State after the run.
    pub state: Value,
    /// Usage from the terminal event; absent counts stay absent (LIFE-33).
    pub usage: Vec<TokenUsage>,
    /// Interrupts the run ended with.
    pub interrupts: Vec<Interrupt>,
    /// Frontend calls left unanswered, derived from the stream (TOOL-32).
    pub pending_tool_calls: Vec<PendingToolCall>,
    /// The diagnostics reported during the run: the first
    /// `Limits::max_report_diagnostics`, then one `diagnostics.limit`
    /// summary if more were reported (the sink and observers saw all).
    pub diagnostics: Vec<Diagnostic>,
}

impl RunReport {
    /// A report with no history, state `{}` and nothing pending, whose
    /// requested run is its only run, for tests
    /// of code that consumes reports (an [`Observer`](super::Observer), a
    /// store). Set the public fields to fill it in. A real run produces
    /// reports through [`AgentClient::run`](super::AgentClient::run); for an
    /// end-to-end test, serve frames from a mock [`Transport`](super::Transport)
    /// with [`FrameStream::from_frames`](super::FrameStream::from_frames).
    ///
    /// ```
    /// use agui_sdk::client::{RunReport, RunStatus};
    ///
    /// let mut report = RunReport::new(
    ///     "t-1".into(),
    ///     "r-1".into(),
    ///     RunStatus::Finished { outcome: None, result: None },
    /// );
    /// report.state = serde_json::json!({ "count": 1 });
    /// assert!(report.status.is_success());
    /// ```
    #[must_use]
    pub fn new(thread_id: ThreadId, run_id: RunId, status: RunStatus) -> Self {
        RunReport {
            thread_id,
            requested_run_id: run_id.clone(),
            requested_status: status.clone(),
            run_id,
            status,
            messages: Vec::new(),
            new_message_ids: Vec::new(),
            state: Value::Object(serde_json::Map::new()),
            usage: Vec::new(),
            interrupts: Vec::new(),
            pending_tool_calls: Vec::new(),
            diagnostics: Vec::new(),
        }
    }

    /// Messages the run added, in order (looked up by
    /// [`new_message_ids`](Self::new_message_ids)).
    pub fn new_messages(&self) -> impl Iterator<Item = &Message> + '_ {
        // One pass over the history collects, per new id, the messages
        // holding it (in history order); then the ids are walked in their own
        // order, so a reordered snapshot keeps the run's order. An id may
        // repeat (a tool result sharing a held non-tool message's id): the
        // n-th repeat resolves to the n-th message holding that id. Linear in the
        // history however many messages the (peer-controlled) run added.
        let mut slots: HashMap<&MessageId, (Vec<&Message>, usize)> = self
            .new_message_ids
            .iter()
            .map(|id| (id, (Vec::new(), 0)))
            .collect();
        for message in &self.messages {
            if let Some((held, _)) = slots.get_mut(message.id()) {
                held.push(message);
            }
        }
        self.new_message_ids.iter().filter_map(move |id| {
            let (held, next) = slots.get_mut(id)?;
            let message = held.get(*next).copied();
            *next += 1;
            message
        })
    }

    /// State as a typed view.
    ///
    /// # Errors
    ///
    /// [`ErrorCode::Json`](crate::ErrorCode::Json) if the state does not deserialize into `S`.
    pub fn state_as<S: DeserializeOwned>(&self) -> Result<S> {
        S::deserialize(&self.state).map_err(|err| {
            Error::new(ErrorCode::Json, "state does not match the requested type").with_source(err)
        })
    }
}

/// A run in progress. Lazy: nothing is sent until it is polled or awaited.
/// `'static` and `Send`: it can be moved to a spawned task.
///
/// * As an async iterator ([`next`](Self::next), [`poll_next`](Self::poll_next);
///   `futures_core::Stream<Item = Result<RunUpdate>>` with feature `stream`)
///   it yields each verified event; after the terminal event it ends, and a
///   failure is one final `Err`.
/// * Awaited (`IntoFuture`), it drives the run to completion and returns the
///   [`RunReport`].
///
/// Between items, [`messages`](Self::messages) and [`state`](Self::state)
/// borrow the reduced view. Dropping the handle closes the connection and
/// still delivers `on_finish` to observers.
#[must_use = "a run does nothing until it is awaited or polled"]
pub struct RunHandle {
    thread_id: ThreadId,
    run_id: RunId,
    cancel: CancelToken,
    driver: Box<RunDriver>,
}

mod driver;

use driver::RunDriver;
pub(super) use driver::{Open, RunSetup, FRAMES_PER_POLL};

// The handle must be movable to a spawned task.
const _: fn() = || {
    fn assert_send_static<T: Send + 'static>() {}
    assert_send_static::<RunHandle>();
    assert_send_static::<RunFuture>();
};

impl RunHandle {
    pub(super) fn new(setup: RunSetup) -> Self {
        let thread_id = setup.thread_id.clone();
        let run_id = setup.run_id.clone();
        let driver = Box::new(RunDriver::new(setup));
        RunHandle {
            thread_id,
            run_id,
            cancel: driver.cancel_token().clone(),
            driver,
        }
    }

    /// The run's thread id.
    #[must_use]
    pub fn thread_id(&self) -> &ThreadId {
        &self.thread_id
    }

    /// The requested run id.
    #[must_use]
    pub fn run_id(&self) -> &RunId {
        &self.run_id
    }

    /// History so far, borrowed: the history sent, then what the run added.
    /// After a successful end it is the report's history; after a failure,
    /// read the error's [`partial`](crate::Error::partial) report.
    #[must_use]
    pub fn messages(&self) -> &[Message] {
        self.driver.messages()
    }

    /// One message by id, O(1) while the run is in progress.
    #[must_use]
    pub fn message(&self, id: &MessageId) -> Option<&Message> {
        self.driver.message(id)
    }

    /// State so far, borrowed.
    #[must_use]
    pub fn state(&self) -> &Value {
        self.driver.state()
    }

    /// Cancels the run. The run ends with `run.cancelled` and a partial
    /// report; no `RUN_FINISHED` is synthesized (LIFE-20). A run whose
    /// terminal event was already received is not overridden: the events
    /// already received are still delivered, and it ends as that terminal
    /// says, without reading the transport further.
    pub fn cancel(&self) {
        self.cancel.cancel();
    }

    /// The run's cancellation token, usable from another task. Cancelling
    /// wakes the transport at once and ends the run with `run.cancelled`.
    #[must_use]
    pub fn cancel_token(&self) -> CancelToken {
        self.cancel.clone()
    }

    /// Polls for the next update.
    pub fn poll_next(&mut self, cx: &mut Context<'_>) -> Poll<Option<Result<RunUpdate>>> {
        self.driver.poll_next(cx)
    }

    /// The next update; `None` once the run has ended.
    // An inherent async `next`, like an async iterator; not `Iterator::next`.
    #[allow(clippy::should_implement_trait)]
    pub fn next(&mut self) -> impl Future<Output = Option<Result<RunUpdate>>> + Send + '_ {
        NextUpdate { handle: self }
    }
}

struct NextUpdate<'a> {
    handle: &'a mut RunHandle,
}

impl Future for NextUpdate<'_> {
    type Output = Option<Result<RunUpdate>>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        self.get_mut().handle.poll_next(cx)
    }
}

#[cfg(feature = "stream")]
#[cfg_attr(docsrs, doc(cfg(feature = "stream")))]
impl futures_core::Stream for RunHandle {
    type Item = Result<RunUpdate>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        RunHandle::poll_next(self.get_mut(), cx)
    }
}

impl IntoFuture for RunHandle {
    type Output = Result<RunReport>;
    type IntoFuture = RunFuture;

    fn into_future(self) -> RunFuture {
        RunFuture { handle: self }
    }
}

impl std::fmt::Debug for RunHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RunHandle")
            .field("thread_id", &self.thread_id)
            .field("run_id", &self.run_id)
            .finish_non_exhaustive()
    }
}

/// Updates a [`RunFuture`] handles in one `poll` before yielding.
const UPDATES_PER_POLL: usize = 256;

/// The future of a whole run, from awaiting a [`RunHandle`].
#[must_use = "a run does nothing until it is awaited"]
pub struct RunFuture {
    handle: RunHandle,
}

impl Future for RunFuture {
    type Output = Result<RunReport>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let handle = &mut self.get_mut().handle;
        let mut budget = UPDATES_PER_POLL;
        loop {
            match handle.poll_next(cx) {
                // Dropped at once, so the reducer can take payloads without
                // copying them.
                Poll::Ready(Some(Ok(_update))) => {
                    // A transport that is always ready must not starve the
                    // executor: yield now and then.
                    budget -= 1;
                    if budget == 0 {
                        cx.waker().wake_by_ref();
                        return Poll::Pending;
                    }
                }
                Poll::Ready(Some(Err(err))) => return Poll::Ready(Err(err)),
                Poll::Ready(None) => {
                    return Poll::Ready(handle.driver.take_report().ok_or_else(|| {
                        Error::new(
                            ErrorCode::InvalidRequest,
                            "the run already ended and its outcome was taken",
                        )
                    }))
                }
                Poll::Pending => return Poll::Pending,
            }
        }
    }
}

impl std::fmt::Debug for RunFuture {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RunFuture")
            .field("handle", &self.handle)
            .finish()
    }
}
