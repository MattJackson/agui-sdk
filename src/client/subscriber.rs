//! A per-event-type callback adapter for applications ported from the
//! TypeScript client's subscriber style.
//!
//! A [`Subscriber`] registers closures by [`EventType`]. Each runs at the
//! application stage, after the reducer, and may replace the state or the
//! history, or stop later subscribers from seeing the event. It is built on
//! the same machinery as [`Observer`](super::Observer), so it never sees
//! unverified material. New code should prefer an `Observer` (read) or
//! [`Middleware`](super::Middleware) (write).
//!
//! # Porting the TypeScript hooks
//!
//! The TypeScript subscriber has about forty named hooks. Here every
//! per-event hook is one registration with [`Subscriber::on`], and the rest
//! map onto the client's other surfaces:
//!
//! | TypeScript hook | Here |
//! |---|---|
//! | `onEvent` | [`Subscriber::on_any`] |
//! | `on<Type>Event` (one per event type: `onRunStartedEvent`, `onTextMessageContentEvent`, `onToolCallArgsEvent`, `onStateDeltaEvent`, …) | `on(EventType::<Type>, …)` |
//! | `onRunErrorEvent`, `onRunFinishedEvent` | `on(EventType::RunError, …)`, `on(EventType::RunFinished, …)` |
//! | `onMessagesChanged`, `onNewMessage`, `onNewToolCall` | [`Observer::on_event`](super::Observer::on_event): `update.changes.message` / `messages_replaced`, `view.messages()` |
//! | `onStateChanged` | [`Observer::on_event`](super::Observer::on_event): `update.changes.state`, `view.state()` |
//! | `onRunInitialized` | [`Observer::on_event`](super::Observer::on_event) for `RUN_STARTED` |
//! | `onRunFailed`, `onRunFinalized` | [`Observer::on_finish`](super::Observer::on_finish) (`Err`, and always) |
//!
//! Returning `{ stopPropagation: true }` is [`Flow::Stop`]; returning
//! `{ state }` or `{ messages }` is [`Mutation::set_state`] or
//! [`Mutation::set_messages`].
//!
//! # What a mutation changes
//!
//! The verified stream's own reduction is never rewritten: later events
//! (deltas, snapshots) keep applying to the state and history the protocol
//! built, so a subscriber cannot make the client accept something the
//! pipeline did not verify. A mutation replaces what the run *reports*:
//! [`RunHandle::messages`](super::RunHandle::messages) and
//! [`RunHandle::state`](super::RunHandle::state) from the next item on, the
//! [`RunReport`](super::RunReport) (and so what a
//! [`Thread`](super::Thread) carries into its next run). The latest mutation
//! wins. A panicking callback fails the run with `observer.panicked`.

use serde_json::Value;

use crate::event::{Event, EventType};
use crate::message::Message;

/// What a callback asks for after handling an event.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub enum Flow {
    /// Let later subscribers see the event.
    #[default]
    Continue,
    /// Stop: later subscribers do not see this event.
    Stop,
}

/// Mutations a callback may request. Applied after the callback returns.
#[derive(Debug, Default)]
pub struct Mutation {
    state: Option<Value>,
    messages: Option<Vec<Message>>,
}

impl Mutation {
    /// Replaces the state.
    pub fn set_state(&mut self, state: Value) {
        self.state = Some(state);
    }

    /// Replaces the history.
    pub fn set_messages(&mut self, messages: Vec<Message>) {
        self.messages = Some(messages);
    }

    /// Takes the requested state replacement.
    pub(super) fn take_state(&mut self) -> Option<Value> {
        self.state.take()
    }

    /// Takes the requested history replacement.
    pub(super) fn take_messages(&mut self) -> Option<Vec<Message>> {
        self.messages.take()
    }
}

type Callback = Box<dyn Fn(&Event, &mut Mutation) -> Flow + Send + Sync>;

/// Closures keyed by event type.
#[derive(Default)]
pub struct Subscriber {
    callbacks: Vec<(Option<EventType>, Callback)>,
}

impl Subscriber {
    /// No callbacks.
    #[must_use]
    pub fn new() -> Self {
        Subscriber::default()
    }

    /// Calls `f` for every event of `event_type`.
    #[must_use]
    pub fn on(
        mut self,
        event_type: EventType,
        f: impl Fn(&Event, &mut Mutation) -> Flow + Send + Sync + 'static,
    ) -> Self {
        self.callbacks.push((Some(event_type), Box::new(f)));
        self
    }

    /// Calls `f` for every event.
    #[must_use]
    pub fn on_any(
        mut self,
        f: impl Fn(&Event, &mut Mutation) -> Flow + Send + Sync + 'static,
    ) -> Self {
        self.callbacks.push((None, Box::new(f)));
        self
    }

    /// Runs the callbacks that match `event`, in registration order, until
    /// one returns [`Flow::Stop`].
    pub(super) fn dispatch(&self, event: &Event, mutation: &mut Mutation) -> Flow {
        let event_type = event.event_type();
        for (filter, callback) in &self.callbacks {
            if filter.map_or(true, |t| t == event_type) && callback(event, mutation) == Flow::Stop {
                return Flow::Stop;
            }
        }
        Flow::Continue
    }
}

impl std::fmt::Debug for Subscriber {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Subscriber")
            .field("callbacks", &self.callbacks.len())
            .finish()
    }
}
