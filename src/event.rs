//! [`Event`]: the 31 protocol events, and [`EventType`].
//!
//! Each event's struct lives in its spec area's module ([`crate::text`],
//! [`crate::tool`], ...); this module gathers them into one enum, tagged by
//! `type` on the wire (`$defs/Event`).
//!
//! # Typed events are post-enforcement
//!
//! A value of type [`Event`] obtained from this crate's client has been through
//! compatibility translation, enforcement, chunk expansion and verification
//! (ARCH-01, PROC-21, PROC-28). Unknown events never become an `Event` (they
//! are dropped with a warning) and unknown fields never reach one (they are
//! stripped), so application code cannot see material the protocol does not
//! describe (PROC-10, PROC-14, PROC-30). Code that must see raw material does
//! so in [`client::middleware`][crate::client::middleware], on JSON, before enforcement.
//!
//! [`Event::parse_strict`] is the authoring and test-time validator (SCHF-01):
//! it rejects unknown material instead of stripping it and is never wired into
//! a receive path.
//!
//! The derived `Deserialize` on [`Event`] (and every protocol type) is
//! *unchecked decoding of trusted, stored data*: it fails on an unknown
//! closed-set value instead of stripping it (D3, PROC-17), drops unknown
//! fields without a warning and verifies nothing. It does not build an
//! intermediate tree for the whole object: members before `type` are held as
//! values and replayed, so when `type` is first (as this crate writes events)
//! nothing is buffered.
//! Use it for your own storage and fixtures. Frames received from a peer go
//! through [`pipeline::Processor`](crate::pipeline::Processor).
#![cfg_attr(
    not(feature = "client"),
    doc = "",
    doc = "[crate::client::middleware]: https://docs.rs/agui-sdk/latest/agui_sdk/client/middleware/index.html"
)]

use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;

use crate::error::Result;
use crate::ids::SubagentRunId;
use crate::json::{deserialize_tagged, Tagged};
use crate::metadata::Metadata;

pub use crate::activity::{ActivityDelta, ActivitySnapshot};
pub use crate::lifecycle::{RunError, RunFinished, RunStarted, StepFinished, StepStarted};
pub use crate::passthrough::{Custom, Raw};
pub use crate::reasoning::{
    ReasoningEncryptedValue, ReasoningEnd, ReasoningMessageChunk, ReasoningMessageContent,
    ReasoningMessageEnd, ReasoningMessageStart, ReasoningStart,
};
pub use crate::state::{MessagesSnapshot, StateDelta, StateSnapshot};
pub use crate::subagent::{SubagentError, SubagentFinished, SubagentStarted};
pub use crate::text::{TextMessageChunk, TextMessageContent, TextMessageEnd, TextMessageStart};
pub use crate::tool::{ToolCallArgs, ToolCallChunk, ToolCallEnd, ToolCallResult, ToolCallStart};

/// One protocol event (`$defs/Event`).
///
/// Serializes to the wire JSON form: `type` first, optional fields omitted
/// rather than `null` (BASIC-09). Deserializes without an intermediate tree:
/// when `type` is the first member, which is how this crate writes events,
/// the event's fields are read straight into their final types.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "SCREAMING_SNAKE_CASE")]
#[non_exhaustive]
pub enum Event {
    /// `RUN_STARTED`: see [`lifecycle::RunStarted`](crate::lifecycle::RunStarted).
    RunStarted(crate::lifecycle::RunStarted),
    /// `RUN_FINISHED`: see [`lifecycle::RunFinished`](crate::lifecycle::RunFinished).
    RunFinished(crate::lifecycle::RunFinished),
    /// `RUN_ERROR`: see [`lifecycle::RunError`](crate::lifecycle::RunError).
    RunError(crate::lifecycle::RunError),
    /// `STEP_STARTED`: see [`lifecycle::StepStarted`](crate::lifecycle::StepStarted).
    StepStarted(crate::lifecycle::StepStarted),
    /// `STEP_FINISHED`: see [`lifecycle::StepFinished`](crate::lifecycle::StepFinished).
    StepFinished(crate::lifecycle::StepFinished),
    /// `TEXT_MESSAGE_START`: see [`text::TextMessageStart`](crate::text::TextMessageStart).
    TextMessageStart(crate::text::TextMessageStart),
    /// `TEXT_MESSAGE_CONTENT`: see [`text::TextMessageContent`](crate::text::TextMessageContent).
    TextMessageContent(crate::text::TextMessageContent),
    /// `TEXT_MESSAGE_END`: see [`text::TextMessageEnd`](crate::text::TextMessageEnd).
    TextMessageEnd(crate::text::TextMessageEnd),
    /// `TEXT_MESSAGE_CHUNK`: see [`text::TextMessageChunk`](crate::text::TextMessageChunk).
    TextMessageChunk(crate::text::TextMessageChunk),
    /// `TOOL_CALL_START`: see [`tool::ToolCallStart`](crate::tool::ToolCallStart).
    ToolCallStart(crate::tool::ToolCallStart),
    /// `TOOL_CALL_ARGS`: see [`tool::ToolCallArgs`](crate::tool::ToolCallArgs).
    ToolCallArgs(crate::tool::ToolCallArgs),
    /// `TOOL_CALL_END`: see [`tool::ToolCallEnd`](crate::tool::ToolCallEnd).
    ToolCallEnd(crate::tool::ToolCallEnd),
    /// `TOOL_CALL_CHUNK`: see [`tool::ToolCallChunk`](crate::tool::ToolCallChunk).
    ToolCallChunk(crate::tool::ToolCallChunk),
    /// `TOOL_CALL_RESULT`: see [`tool::ToolCallResult`](crate::tool::ToolCallResult).
    ToolCallResult(crate::tool::ToolCallResult),
    /// `STATE_SNAPSHOT`: see [`state::StateSnapshot`](crate::state::StateSnapshot).
    StateSnapshot(crate::state::StateSnapshot),
    /// `STATE_DELTA`: see [`state::StateDelta`](crate::state::StateDelta).
    StateDelta(crate::state::StateDelta),
    /// `MESSAGES_SNAPSHOT`: see [`state::MessagesSnapshot`](crate::state::MessagesSnapshot).
    MessagesSnapshot(crate::state::MessagesSnapshot),
    /// `ACTIVITY_SNAPSHOT`: see [`activity::ActivitySnapshot`](crate::activity::ActivitySnapshot).
    ActivitySnapshot(crate::activity::ActivitySnapshot),
    /// `ACTIVITY_DELTA`: see [`activity::ActivityDelta`](crate::activity::ActivityDelta).
    ActivityDelta(crate::activity::ActivityDelta),
    /// `REASONING_START`: see [`reasoning::ReasoningStart`](crate::reasoning::ReasoningStart).
    ReasoningStart(crate::reasoning::ReasoningStart),
    /// `REASONING_MESSAGE_START`: see [`reasoning::ReasoningMessageStart`](crate::reasoning::ReasoningMessageStart).
    ReasoningMessageStart(crate::reasoning::ReasoningMessageStart),
    /// `REASONING_MESSAGE_CONTENT`: see [`reasoning::ReasoningMessageContent`](crate::reasoning::ReasoningMessageContent).
    ReasoningMessageContent(crate::reasoning::ReasoningMessageContent),
    /// `REASONING_MESSAGE_END`: see [`reasoning::ReasoningMessageEnd`](crate::reasoning::ReasoningMessageEnd).
    ReasoningMessageEnd(crate::reasoning::ReasoningMessageEnd),
    /// `REASONING_MESSAGE_CHUNK`: see [`reasoning::ReasoningMessageChunk`](crate::reasoning::ReasoningMessageChunk).
    ReasoningMessageChunk(crate::reasoning::ReasoningMessageChunk),
    /// `REASONING_END`: see [`reasoning::ReasoningEnd`](crate::reasoning::ReasoningEnd).
    ReasoningEnd(crate::reasoning::ReasoningEnd),
    /// `REASONING_ENCRYPTED_VALUE`: see [`reasoning::ReasoningEncryptedValue`](crate::reasoning::ReasoningEncryptedValue).
    ReasoningEncryptedValue(crate::reasoning::ReasoningEncryptedValue),
    /// `SUBAGENT_STARTED`: see [`subagent::SubagentStarted`](crate::subagent::SubagentStarted).
    SubagentStarted(crate::subagent::SubagentStarted),
    /// `SUBAGENT_FINISHED`: see [`subagent::SubagentFinished`](crate::subagent::SubagentFinished).
    SubagentFinished(crate::subagent::SubagentFinished),
    /// `SUBAGENT_ERROR`: see [`subagent::SubagentError`](crate::subagent::SubagentError).
    SubagentError(crate::subagent::SubagentError),
    /// `RAW`: see [`passthrough::Raw`](crate::passthrough::Raw).
    Raw(crate::passthrough::Raw),
    /// `CUSTOM`: see [`passthrough::Custom`](crate::passthrough::Custom).
    Custom(crate::passthrough::Custom),
}

/// Calls `$callback!($args; <every variant name>)`. Variant names equal both
/// the `EventType` names and the event struct names, so one list drives the
/// per-variant projections on `Event` below (`event_type`, `timestamp`,
/// `metadata`, `raw_event`); `EventType`'s tables are hand-written and checked
/// by `tests/basic_events.rs`.
macro_rules! all_events {
    ($callback:ident ! ( $($args:tt)* )) => {
        $callback! {
            $($args)*;
            RunStarted RunFinished RunError StepStarted StepFinished
            TextMessageStart TextMessageContent TextMessageEnd TextMessageChunk
            ToolCallStart ToolCallArgs ToolCallEnd ToolCallChunk ToolCallResult
            StateSnapshot StateDelta MessagesSnapshot ActivitySnapshot ActivityDelta
            ReasoningStart ReasoningMessageStart ReasoningMessageContent ReasoningMessageEnd
            ReasoningMessageChunk ReasoningEnd ReasoningEncryptedValue
            SubagentStarted SubagentFinished SubagentError Raw Custom
        }
    };
}

/// `match` over every variant, evaluating `$body` with `$e` bound to the
/// event struct. Invoked through [`all_events!`].
macro_rules! project {
    ($event:expr, $e:ident, $body:expr; $($name:ident)*) => {
        match $event { $(Event::$name($e) => $body,)* }
    };
}

impl Event {
    /// The event's `type`.
    #[must_use]
    pub fn event_type(&self) -> EventType {
        macro_rules! type_of {
            ($event:expr; $($name:ident)*) => {
                match $event { $(Event::$name(_) => EventType::$name,)* }
            };
        }
        all_events!(type_of!(self))
    }

    /// The producer timestamp, if sent. Informational only (BASIC-04).
    #[must_use]
    pub fn timestamp(&self) -> Option<i64> {
        all_events!(project!(self, e, e.timestamp))
    }

    /// The event's own metadata, if any.
    #[must_use]
    pub fn metadata(&self) -> Option<&Metadata> {
        all_events!(project!(self, e, e.metadata.as_ref()))
    }

    /// The subagent attribution for attributable events (SUB-01). `None` for
    /// the parent agent and for run-scoped events, which carry no attribution
    /// (BASIC-08). For `SUBAGENT_*` events this is the invocation they concern.
    #[must_use]
    pub fn subagent_run_id(&self) -> Option<&SubagentRunId> {
        match self {
            Event::RunStarted(_)
            | Event::RunFinished(_)
            | Event::RunError(_)
            | Event::MessagesSnapshot(_) => None,
            Event::SubagentStarted(e) => Some(&e.subagent_run_id),
            Event::SubagentFinished(e) => Some(&e.subagent_run_id),
            Event::SubagentError(e) => Some(&e.subagent_run_id),
            Event::StepStarted(e) => e.subagent_run_id.as_ref(),
            Event::StepFinished(e) => e.subagent_run_id.as_ref(),
            Event::TextMessageStart(e) => e.subagent_run_id.as_ref(),
            Event::TextMessageContent(e) => e.subagent_run_id.as_ref(),
            Event::TextMessageEnd(e) => e.subagent_run_id.as_ref(),
            Event::TextMessageChunk(e) => e.subagent_run_id.as_ref(),
            Event::ToolCallStart(e) => e.subagent_run_id.as_ref(),
            Event::ToolCallArgs(e) => e.subagent_run_id.as_ref(),
            Event::ToolCallEnd(e) => e.subagent_run_id.as_ref(),
            Event::ToolCallChunk(e) => e.subagent_run_id.as_ref(),
            Event::ToolCallResult(e) => e.subagent_run_id.as_ref(),
            Event::StateSnapshot(e) => e.subagent_run_id.as_ref(),
            Event::StateDelta(e) => e.subagent_run_id.as_ref(),
            Event::ActivitySnapshot(e) => e.subagent_run_id.as_ref(),
            Event::ActivityDelta(e) => e.subagent_run_id.as_ref(),
            Event::ReasoningStart(e) => e.subagent_run_id.as_ref(),
            Event::ReasoningMessageStart(e) => e.subagent_run_id.as_ref(),
            Event::ReasoningMessageContent(e) => e.subagent_run_id.as_ref(),
            Event::ReasoningMessageEnd(e) => e.subagent_run_id.as_ref(),
            Event::ReasoningMessageChunk(e) => e.subagent_run_id.as_ref(),
            Event::ReasoningEnd(e) => e.subagent_run_id.as_ref(),
            Event::ReasoningEncryptedValue(e) => e.subagent_run_id.as_ref(),
            Event::Raw(e) => e.subagent_run_id.as_ref(),
            Event::Custom(e) => e.subagent_run_id.as_ref(),
        }
    }

    /// The provider-native `rawEvent`, if carried. Never consulted by the SDK.
    #[must_use]
    pub fn raw_event(&self) -> Option<&Value> {
        all_events!(project!(self, e, e.raw_event.as_ref()))
    }

    /// Whether this is `RUN_FINISHED` or `RUN_ERROR`.
    #[must_use]
    pub fn is_run_terminal(&self) -> bool {
        matches!(self, Event::RunFinished(_) | Event::RunError(_))
    }

    /// Strict validation for authoring and tests (SCHF-01): rejects unknown
    /// events, fields, union members and enum values instead of stripping them,
    /// as the strict schema does. Not a receive path.
    ///
    /// # Errors
    ///
    /// [`ErrorCode::InvalidValue`](crate::ErrorCode::InvalidValue) with the
    /// offending JSON pointer;
    /// [`ErrorCode::MalformedEvent`](crate::ErrorCode::MalformedEvent) (no
    /// pointer) for bytes that are not JSON;
    /// [`ErrorCode::LimitExceeded`](crate::ErrorCode::LimitExceeded) for a
    /// document over the entry or depth budget.
    pub fn parse_strict(json: &[u8]) -> Result<Self> {
        crate::pipeline::enforce::parse_strict_event(json)
    }

    /// Wire JSON for this event, written into `out` (cleared first) so a
    /// caller encoding many events reuses one buffer.
    pub fn write_json(&self, out: &mut Vec<u8>) {
        out.clear();
        // Writing into a `Vec` cannot fail, and every map key in the model
        // is a string, so serialization has no error path in practice.
        if serde_json::to_writer(&mut *out, self).is_err() {
            out.clear();
        }
    }
}

impl Tagged for Event {
    const TAG: &'static str = "type";
    const EXPECTING: &'static str = "an AG-UI event object";
    type Tag = EventType;

    fn from_tag<'de, D: Deserializer<'de>>(tag: EventType, rest: D) -> Result<Self, D::Error> {
        // Each struct reads the remaining members; the enum just names it.
        macro_rules! read_as {
            ($tag:expr, $rest:expr; $($name:ident)*) => {
                match $tag { $(EventType::$name => Event::$name(Deserialize::deserialize($rest)?),)* }
            };
        }
        Ok(all_events!(read_as!(tag, rest)))
    }
}

impl<'de> Deserialize<'de> for Event {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserialize_tagged(deserializer)
    }
}

/// The 31 event types (`$defs/EventType`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
#[non_exhaustive]
pub enum EventType {
    /// `RUN_STARTED`.
    RunStarted,
    /// `RUN_FINISHED`.
    RunFinished,
    /// `RUN_ERROR`.
    RunError,
    /// `STEP_STARTED`.
    StepStarted,
    /// `STEP_FINISHED`.
    StepFinished,
    /// `TEXT_MESSAGE_START`.
    TextMessageStart,
    /// `TEXT_MESSAGE_CONTENT`.
    TextMessageContent,
    /// `TEXT_MESSAGE_END`.
    TextMessageEnd,
    /// `TEXT_MESSAGE_CHUNK`.
    TextMessageChunk,
    /// `TOOL_CALL_START`.
    ToolCallStart,
    /// `TOOL_CALL_ARGS`.
    ToolCallArgs,
    /// `TOOL_CALL_END`.
    ToolCallEnd,
    /// `TOOL_CALL_CHUNK`.
    ToolCallChunk,
    /// `TOOL_CALL_RESULT`.
    ToolCallResult,
    /// `STATE_SNAPSHOT`.
    StateSnapshot,
    /// `STATE_DELTA`.
    StateDelta,
    /// `MESSAGES_SNAPSHOT`.
    MessagesSnapshot,
    /// `ACTIVITY_SNAPSHOT`.
    ActivitySnapshot,
    /// `ACTIVITY_DELTA`.
    ActivityDelta,
    /// `REASONING_START`.
    ReasoningStart,
    /// `REASONING_MESSAGE_START`.
    ReasoningMessageStart,
    /// `REASONING_MESSAGE_CONTENT`.
    ReasoningMessageContent,
    /// `REASONING_MESSAGE_END`.
    ReasoningMessageEnd,
    /// `REASONING_MESSAGE_CHUNK`.
    ReasoningMessageChunk,
    /// `REASONING_END`.
    ReasoningEnd,
    /// `REASONING_ENCRYPTED_VALUE`.
    ReasoningEncryptedValue,
    /// `SUBAGENT_STARTED`.
    SubagentStarted,
    /// `SUBAGENT_FINISHED`.
    SubagentFinished,
    /// `SUBAGENT_ERROR`.
    SubagentError,
    /// `RAW`.
    Raw,
    /// `CUSTOM`.
    Custom,
}

impl EventType {
    /// Every event type this build knows, in declaration order (lifecycle
    /// first, the order of the [`Event`] enum).
    pub const ALL: &'static [EventType] = &[
        EventType::RunStarted,
        EventType::RunFinished,
        EventType::RunError,
        EventType::StepStarted,
        EventType::StepFinished,
        EventType::TextMessageStart,
        EventType::TextMessageContent,
        EventType::TextMessageEnd,
        EventType::TextMessageChunk,
        EventType::ToolCallStart,
        EventType::ToolCallArgs,
        EventType::ToolCallEnd,
        EventType::ToolCallChunk,
        EventType::ToolCallResult,
        EventType::StateSnapshot,
        EventType::StateDelta,
        EventType::MessagesSnapshot,
        EventType::ActivitySnapshot,
        EventType::ActivityDelta,
        EventType::ReasoningStart,
        EventType::ReasoningMessageStart,
        EventType::ReasoningMessageContent,
        EventType::ReasoningMessageEnd,
        EventType::ReasoningMessageChunk,
        EventType::ReasoningEnd,
        EventType::ReasoningEncryptedValue,
        EventType::SubagentStarted,
        EventType::SubagentFinished,
        EventType::SubagentError,
        EventType::Raw,
        EventType::Custom,
    ];

    /// The wire spelling, e.g. `"TEXT_MESSAGE_START"`.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            EventType::RunStarted => "RUN_STARTED",
            EventType::RunFinished => "RUN_FINISHED",
            EventType::RunError => "RUN_ERROR",
            EventType::StepStarted => "STEP_STARTED",
            EventType::StepFinished => "STEP_FINISHED",
            EventType::TextMessageStart => "TEXT_MESSAGE_START",
            EventType::TextMessageContent => "TEXT_MESSAGE_CONTENT",
            EventType::TextMessageEnd => "TEXT_MESSAGE_END",
            EventType::TextMessageChunk => "TEXT_MESSAGE_CHUNK",
            EventType::ToolCallStart => "TOOL_CALL_START",
            EventType::ToolCallArgs => "TOOL_CALL_ARGS",
            EventType::ToolCallEnd => "TOOL_CALL_END",
            EventType::ToolCallChunk => "TOOL_CALL_CHUNK",
            EventType::ToolCallResult => "TOOL_CALL_RESULT",
            EventType::StateSnapshot => "STATE_SNAPSHOT",
            EventType::StateDelta => "STATE_DELTA",
            EventType::MessagesSnapshot => "MESSAGES_SNAPSHOT",
            EventType::ActivitySnapshot => "ACTIVITY_SNAPSHOT",
            EventType::ActivityDelta => "ACTIVITY_DELTA",
            EventType::ReasoningStart => "REASONING_START",
            EventType::ReasoningMessageStart => "REASONING_MESSAGE_START",
            EventType::ReasoningMessageContent => "REASONING_MESSAGE_CONTENT",
            EventType::ReasoningMessageEnd => "REASONING_MESSAGE_END",
            EventType::ReasoningMessageChunk => "REASONING_MESSAGE_CHUNK",
            EventType::ReasoningEnd => "REASONING_END",
            EventType::ReasoningEncryptedValue => "REASONING_ENCRYPTED_VALUE",
            EventType::SubagentStarted => "SUBAGENT_STARTED",
            EventType::SubagentFinished => "SUBAGENT_FINISHED",
            EventType::SubagentError => "SUBAGENT_ERROR",
            EventType::Raw => "RAW",
            EventType::Custom => "CUSTOM",
        }
    }

    /// Parses a wire spelling. `None` for a type this build does not know:
    /// the pipeline drops such an event with a warning (PROC-01, PROC-12).
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        Some(match text {
            "RUN_STARTED" => EventType::RunStarted,
            "RUN_FINISHED" => EventType::RunFinished,
            "RUN_ERROR" => EventType::RunError,
            "STEP_STARTED" => EventType::StepStarted,
            "STEP_FINISHED" => EventType::StepFinished,
            "TEXT_MESSAGE_START" => EventType::TextMessageStart,
            "TEXT_MESSAGE_CONTENT" => EventType::TextMessageContent,
            "TEXT_MESSAGE_END" => EventType::TextMessageEnd,
            "TEXT_MESSAGE_CHUNK" => EventType::TextMessageChunk,
            "TOOL_CALL_START" => EventType::ToolCallStart,
            "TOOL_CALL_ARGS" => EventType::ToolCallArgs,
            "TOOL_CALL_END" => EventType::ToolCallEnd,
            "TOOL_CALL_CHUNK" => EventType::ToolCallChunk,
            "TOOL_CALL_RESULT" => EventType::ToolCallResult,
            "STATE_SNAPSHOT" => EventType::StateSnapshot,
            "STATE_DELTA" => EventType::StateDelta,
            "MESSAGES_SNAPSHOT" => EventType::MessagesSnapshot,
            "ACTIVITY_SNAPSHOT" => EventType::ActivitySnapshot,
            "ACTIVITY_DELTA" => EventType::ActivityDelta,
            "REASONING_START" => EventType::ReasoningStart,
            "REASONING_MESSAGE_START" => EventType::ReasoningMessageStart,
            "REASONING_MESSAGE_CONTENT" => EventType::ReasoningMessageContent,
            "REASONING_MESSAGE_END" => EventType::ReasoningMessageEnd,
            "REASONING_MESSAGE_CHUNK" => EventType::ReasoningMessageChunk,
            "REASONING_END" => EventType::ReasoningEnd,
            "REASONING_ENCRYPTED_VALUE" => EventType::ReasoningEncryptedValue,
            "SUBAGENT_STARTED" => EventType::SubagentStarted,
            "SUBAGENT_FINISHED" => EventType::SubagentFinished,
            "SUBAGENT_ERROR" => EventType::SubagentError,
            "RAW" => EventType::Raw,
            "CUSTOM" => EventType::Custom,
            _ => return None,
        })
    }

    /// Whether the type is a `*_CHUNK` shorthand the pipeline expands (STRM-07).
    #[must_use]
    pub const fn is_chunk(self) -> bool {
        matches!(
            self,
            EventType::TextMessageChunk
                | EventType::ToolCallChunk
                | EventType::ReasoningMessageChunk
        )
    }

    /// Whether the type is run-scoped and carries no attribution (BASIC-08).
    #[must_use]
    pub const fn is_run_scoped(self) -> bool {
        matches!(
            self,
            EventType::RunStarted
                | EventType::RunFinished
                | EventType::RunError
                | EventType::MessagesSnapshot
        )
    }
}

// Size budget (docs/API.md §8): every move, queue slot and `Vec<Processed>`
// slot pays for the largest variant. Measured on 64-bit: 200 bytes with
// serde_json's default map; 320-336 with `preserve_order` (the IndexMap
// layout varies by version, and another crate can switch the feature on
// through unification), so the hard cap is 384, the pre-revision size, and
// the 200-byte target is checked by the size bench. Large, rare payloads
// (`RunStarted.input`) are boxed.
#[cfg(target_pointer_width = "64")]
const _: () = assert!(std::mem::size_of::<Event>() <= 384);
#[cfg(target_pointer_width = "64")]
const _: () = assert!(std::mem::size_of::<crate::error::Error>() == 8);
