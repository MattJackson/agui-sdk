//! Run lifecycle and steps (`events/lifecycle.mdx`).
//!
//! A stream is one or more runs. Each run opens with [`RunStarted`] (or fails
//! before it begins with [`RunError`]) and closes with exactly one terminal:
//! [`RunFinished`] or [`RunError`] (LIFE-01..LIFE-05). After the terminal only a
//! new `RUN_STARTED` or a late `RUN_ERROR` may follow (LIFE-37..LIFE-42).
//! Steps ([`StepStarted`]/[`StepFinished`]) label spans of a run and may overlap.
//!
//! The client verifier enforces the consumer side of these rules
//! ([`crate::pipeline::verify::ConsumerVerifier`]); on the server the
//! [`Runner`][crate::server::Runner] emits `RUN_STARTED` and the terminal itself
//! and agents return a [`RunOutcome`][crate::server::RunOutcome] instead.
#![cfg_attr(
    not(feature = "server"),
    doc = "",
    doc = "[crate::server::Runner]: https://docs.rs/agui-sdk/latest/agui_sdk/server/struct.Runner.html"
)]
#![cfg_attr(
    not(feature = "server"),
    doc = "",
    doc = "[crate::server::RunOutcome]: https://docs.rs/agui-sdk/latest/agui_sdk/server/struct.RunOutcome.html"
)]

use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;

use crate::ids::{RunId, SubagentRunId, ThreadId, ToolCallId};
use crate::interrupt::Interrupt;
use crate::json::{deserialize_tagged, skip_rest, Tagged};
use crate::metadata::Metadata;
use crate::run_input::RunAgentInput;
use crate::usage::TokenUsage;

/// How a run that did not fail ended (`$defs/RunFinishedOutcome`).
///
/// An absent outcome on the wire means [`Success`](Self::Success) (LIFE-05,
/// CHG-06). An outcome value this SDK does not recognise is stripped by
/// enforcement with a warning, which also reads as success (LIFE-09, PROC-03).
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "lowercase")]
#[non_exhaustive]
pub enum RunFinishedOutcome {
    /// The run completed. It may leave frontend tool calls unanswered and name
    /// them (LIFE-06, TOOL-25..TOOL-31). Clients never read an absent or empty
    /// list as "nothing pending": pending calls are derived from the stream
    /// (TOOL-32).
    #[serde(rename_all = "camelCase")]
    #[non_exhaustive]
    Success {
        /// Unanswered frontend calls, in the order they were made (TOOL-29, TOOL-31).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pending_tool_call_ids: Option<Vec<ToolCallId>>,
    },
    /// The run stopped to ask for outside input (INTR-01, INTR-02).
    #[non_exhaustive]
    Interrupt {
        /// At least one interrupt, ids unique within the run (INTR-04).
        interrupts: Vec<Interrupt>,
    },
    /// The run was stopped before completion for a non-failure reason (LIFE-10).
    /// Neither a success nor a failure (LIFE-17, LIFE-18).
    Cancelled,
}

impl RunFinishedOutcome {
    /// A success outcome with no pending calls listed.
    #[must_use]
    pub fn success() -> Self {
        RunFinishedOutcome::Success {
            pending_tool_call_ids: None,
        }
    }

    /// A success outcome naming unanswered frontend calls (LIFE-06).
    pub fn success_with_pending(ids: impl IntoIterator<Item = ToolCallId>) -> Self {
        RunFinishedOutcome::Success {
            pending_tool_call_ids: Some(ids.into_iter().collect()),
        }
    }

    /// An interrupt outcome. The list must be non-empty; an empty list is
    /// reported by the producer verifier, not here.
    pub fn interrupt(interrupts: impl IntoIterator<Item = Interrupt>) -> Self {
        RunFinishedOutcome::Interrupt {
            interrupts: interrupts.into_iter().collect(),
        }
    }

    /// The cancelled outcome (LIFE-10).
    #[must_use]
    pub fn cancelled() -> Self {
        RunFinishedOutcome::Cancelled
    }
}

/// Wire spellings of [`RunFinishedOutcome`]'s `type`.
#[derive(Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum OutcomeKind {
    Success,
    Interrupt,
    Cancelled,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SuccessFields {
    #[serde(default)]
    pending_tool_call_ids: Option<Vec<ToolCallId>>,
}

#[derive(Deserialize)]
struct InterruptFields {
    interrupts: Vec<Interrupt>,
}

impl Tagged for RunFinishedOutcome {
    const TAG: &'static str = "type";
    const EXPECTING: &'static str = "a run outcome object";
    type Tag = OutcomeKind;

    fn from_tag<'de, D: Deserializer<'de>>(tag: OutcomeKind, rest: D) -> Result<Self, D::Error> {
        Ok(match tag {
            OutcomeKind::Success => {
                let fields = SuccessFields::deserialize(rest)?;
                RunFinishedOutcome::Success {
                    pending_tool_call_ids: fields.pending_tool_call_ids,
                }
            }
            OutcomeKind::Interrupt => RunFinishedOutcome::Interrupt {
                interrupts: InterruptFields::deserialize(rest)?.interrupts,
            },
            OutcomeKind::Cancelled => {
                skip_rest(rest)?;
                RunFinishedOutcome::Cancelled
            }
        })
    }
}

impl<'de> Deserialize<'de> for RunFinishedOutcome {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserialize_tagged(deserializer)
    }
}

/// Opens a run. The first event of every run on a stream (LIFE-01).
///
/// On the server the [`Runner`][crate::server::Runner] emits this itself; agent
/// code never constructs it.
///
/// Schema: `$defs/RunStartedEvent`. Spec rules: LIFE-01, LIFE-04, RUNIN-02, VER-18.
#[cfg_attr(
    not(feature = "server"),
    doc = "",
    doc = "[crate::server::Runner]: https://docs.rs/agui-sdk/latest/agui_sdk/server/struct.Runner.html"
)]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct RunStarted {
    /// Conversation the run belongs to; echoes the input's thread (RUNIN-04).
    pub thread_id: ThreadId,
    /// Identifier of this run; the terminal event must carry the same value (RUNIN-05).
    pub run_id: RunId,
    /// The producer's own protocol version, never an echo of the input's (VER-18).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub protocol_version: Option<String>,
    /// The run that spawned this one as a separate run (RUNIN-07, LIFE-35).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_run_id: Option<RunId>,
    /// Optional echo of the input that requested the run (RUNIN-02). Off by
    /// default on the server: inputs can carry secrets.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    ///
    /// Boxed: the echo is rare and large, and keeping it out of line keeps
    /// [`Event`](crate::Event) small for every other event.
    pub input: Option<Box<RunAgentInput>>,
    /// Producer clock reading, conventionally milliseconds since the Unix epoch.
    ///
    /// Informational only: arrival order is the protocol order and this SDK never
    /// reorders or validates events by it (BASIC-03, BASIC-04).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timestamp: Option<i64>,
    /// The provider-native event this one was translated from, carried verbatim.
    ///
    /// Never consulted by the pipeline (BASIC-05, BASIC-06). Absent rather than
    /// `null` on the wire (BASIC-09).
    #[serde(default, skip_serializing_if = "crate::json::is_absent")]
    pub raw_event: Option<Value>,
    /// Open, vendor-prefixed annotations. Merged key by key into the item this
    /// event contributes to, where the event has one (META-06..META-15).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<Metadata>,
}

impl RunStarted {
    /// Creates the value from its required fields (`thread_id`, `run_id`); every optional
    /// field starts absent.
    pub fn new(thread_id: impl Into<ThreadId>, run_id: impl Into<RunId>) -> Self {
        Self {
            thread_id: thread_id.into(),
            run_id: run_id.into(),
            protocol_version: None,
            parent_run_id: None,
            input: None,
            timestamp: None,
            raw_event: None,
            metadata: None,
        }
    }

    /// Sets [`protocol_version`](Self::protocol_version).
    #[must_use]
    pub fn with_protocol_version(self, protocol_version: impl Into<String>) -> Self {
        Self {
            protocol_version: Some(protocol_version.into()),
            ..self
        }
    }

    /// Sets [`parent_run_id`](Self::parent_run_id).
    #[must_use]
    pub fn with_parent_run_id(self, parent_run_id: impl Into<RunId>) -> Self {
        Self {
            parent_run_id: Some(parent_run_id.into()),
            ..self
        }
    }

    /// Sets [`input`](Self::input).
    #[must_use]
    pub fn with_input(self, input: RunAgentInput) -> Self {
        Self {
            input: Some(Box::new(input)),
            ..self
        }
    }

    /// Sets [`timestamp`](Self::timestamp).
    #[must_use]
    pub fn with_timestamp(self, timestamp: i64) -> Self {
        Self {
            timestamp: Some(timestamp),
            ..self
        }
    }

    /// Sets [`raw_event`](Self::raw_event).
    #[must_use]
    pub fn with_raw_event(self, raw_event: Value) -> Self {
        Self {
            raw_event: crate::json::present(raw_event),
            ..self
        }
    }

    /// Sets [`metadata`](Self::metadata).
    #[must_use]
    pub fn with_metadata(self, metadata: Metadata) -> Self {
        Self {
            metadata: Some(metadata),
            ..self
        }
    }
}

impl From<RunStarted> for crate::event::Event {
    fn from(event: RunStarted) -> Self {
        Self::RunStarted(event)
    }
}

/// Closes a run that did not fail. `outcome` distinguishes completed,
/// interrupted and cancelled runs; absent means success (LIFE-05).
///
/// Schema: `$defs/RunFinishedEvent`. Spec rules: LIFE-05..LIFE-16, LIFE-37.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct RunFinished {
    /// Conversation the run belongs to (RUNIN-04).
    pub thread_id: ThreadId,
    /// Must equal the `RUN_STARTED` run id (RUNIN-05).
    pub run_id: RunId,
    /// The run's return value, if any. Absent (never `null`) when there is none,
    /// and absent on a cancelled run (LIFE-08, LIFE-14).
    #[serde(default, skip_serializing_if = "crate::json::is_absent")]
    pub result: Option<Value>,
    /// How the run ended. Absent is read as success (LIFE-05, LIFE-09, CHG-06).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outcome: Option<RunFinishedOutcome>,
    /// Token accounting, one entry per provider and model (LIFE-08, LIFE-25..36).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<Vec<TokenUsage>>,
    /// Producer clock reading, conventionally milliseconds since the Unix epoch.
    ///
    /// Informational only: arrival order is the protocol order and this SDK never
    /// reorders or validates events by it (BASIC-03, BASIC-04).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timestamp: Option<i64>,
    /// The provider-native event this one was translated from, carried verbatim.
    ///
    /// Never consulted by the pipeline (BASIC-05, BASIC-06). Absent rather than
    /// `null` on the wire (BASIC-09).
    #[serde(default, skip_serializing_if = "crate::json::is_absent")]
    pub raw_event: Option<Value>,
    /// Open, vendor-prefixed annotations. Merged key by key into the item this
    /// event contributes to, where the event has one (META-06..META-15).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<Metadata>,
}

impl RunFinished {
    /// Creates the value from its required fields (`thread_id`, `run_id`); every optional
    /// field starts absent.
    pub fn new(thread_id: impl Into<ThreadId>, run_id: impl Into<RunId>) -> Self {
        Self {
            thread_id: thread_id.into(),
            run_id: run_id.into(),
            result: None,
            outcome: None,
            usage: None,
            timestamp: None,
            raw_event: None,
            metadata: None,
        }
    }

    /// Sets [`result`](Self::result).
    #[must_use]
    pub fn with_result(self, result: Value) -> Self {
        Self {
            result: crate::json::present(result),
            ..self
        }
    }

    /// Sets [`outcome`](Self::outcome).
    #[must_use]
    pub fn with_outcome(self, outcome: RunFinishedOutcome) -> Self {
        Self {
            outcome: Some(outcome),
            ..self
        }
    }

    /// Sets [`usage`](Self::usage).
    #[must_use]
    pub fn with_usage(self, usage: impl IntoIterator<Item = TokenUsage>) -> Self {
        Self {
            usage: Some(usage.into_iter().collect()),
            ..self
        }
    }

    /// Sets [`timestamp`](Self::timestamp).
    #[must_use]
    pub fn with_timestamp(self, timestamp: i64) -> Self {
        Self {
            timestamp: Some(timestamp),
            ..self
        }
    }

    /// Sets [`raw_event`](Self::raw_event).
    #[must_use]
    pub fn with_raw_event(self, raw_event: Value) -> Self {
        Self {
            raw_event: crate::json::present(raw_event),
            ..self
        }
    }

    /// Sets [`metadata`](Self::metadata).
    #[must_use]
    pub fn with_metadata(self, metadata: Metadata) -> Self {
        Self {
            metadata: Some(metadata),
            ..self
        }
    }
}

impl From<RunFinished> for crate::event::Event {
    fn from(event: RunFinished) -> Self {
        Self::RunFinished(event)
    }
}

/// Ends a run in failure. Admitted first (the run failed before it began),
/// mid-run, and after `RUN_FINISHED` as a late failure (LIFE-01, LIFE-39).
///
/// A delivered `RUN_ERROR` is an outcome the client reports, not a stream
/// rejection; the two are never presented as one another (LIFE-54).
///
/// Schema: `$defs/RunErrorEvent`. Spec rules: LIFE-21..LIFE-24, LIFE-39..LIFE-42.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct RunError {
    /// Human-readable failure description.
    pub message: String,
    /// Open, producer-defined failure code; the protocol defines no vocabulary (LIFE-21).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
    /// Tokens spent before the failure (LIFE-21).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<Vec<TokenUsage>>,
    /// Producer clock reading, conventionally milliseconds since the Unix epoch.
    ///
    /// Informational only: arrival order is the protocol order and this SDK never
    /// reorders or validates events by it (BASIC-03, BASIC-04).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timestamp: Option<i64>,
    /// The provider-native event this one was translated from, carried verbatim.
    ///
    /// Never consulted by the pipeline (BASIC-05, BASIC-06). Absent rather than
    /// `null` on the wire (BASIC-09).
    #[serde(default, skip_serializing_if = "crate::json::is_absent")]
    pub raw_event: Option<Value>,
    /// Open, vendor-prefixed annotations. Merged key by key into the item this
    /// event contributes to, where the event has one (META-06..META-15).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<Metadata>,
}

impl RunError {
    /// Creates the value from its required fields (`message`); every optional
    /// field starts absent.
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            code: None,
            usage: None,
            timestamp: None,
            raw_event: None,
            metadata: None,
        }
    }

    /// Sets [`code`](Self::code).
    #[must_use]
    pub fn with_code(self, code: impl Into<String>) -> Self {
        Self {
            code: Some(code.into()),
            ..self
        }
    }

    /// Sets [`usage`](Self::usage).
    #[must_use]
    pub fn with_usage(self, usage: impl IntoIterator<Item = TokenUsage>) -> Self {
        Self {
            usage: Some(usage.into_iter().collect()),
            ..self
        }
    }

    /// Sets [`timestamp`](Self::timestamp).
    #[must_use]
    pub fn with_timestamp(self, timestamp: i64) -> Self {
        Self {
            timestamp: Some(timestamp),
            ..self
        }
    }

    /// Sets [`raw_event`](Self::raw_event).
    #[must_use]
    pub fn with_raw_event(self, raw_event: Value) -> Self {
        Self {
            raw_event: crate::json::present(raw_event),
            ..self
        }
    }

    /// Sets [`metadata`](Self::metadata).
    #[must_use]
    pub fn with_metadata(self, metadata: Metadata) -> Self {
        Self {
            metadata: Some(metadata),
            ..self
        }
    }
}

impl From<RunError> for crate::event::Event {
    fn from(event: RunError) -> Self {
        Self::RunError(event)
    }
}

/// Opens a named step: a label over a span of the run, not a container
/// (LIFE-52). Steps may overlap each other and anything else.
///
/// Schema: `$defs/StepStartedEvent`. Spec rules: LIFE-49..LIFE-53, SUB-22.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct StepStarted {
    /// Step label. Must not already be open for the same owner (LIFE-49, SUB-23).
    pub step_name: String,
    /// Producer clock reading, conventionally milliseconds since the Unix epoch.
    ///
    /// Informational only: arrival order is the protocol order and this SDK never
    /// reorders or validates events by it (BASIC-03, BASIC-04).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timestamp: Option<i64>,
    /// The provider-native event this one was translated from, carried verbatim.
    ///
    /// Never consulted by the pipeline (BASIC-05, BASIC-06). Absent rather than
    /// `null` on the wire (BASIC-09).
    #[serde(default, skip_serializing_if = "crate::json::is_absent")]
    pub raw_event: Option<Value>,
    /// Open, vendor-prefixed annotations. Merged key by key into the item this
    /// event contributes to, where the event has one (META-06..META-15).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<Metadata>,
    /// The subagent invocation that produced this event; absent means the parent
    /// agent (SUB-01). Continuations must agree with their opener (SUB-18, SUB-24).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subagent_run_id: Option<SubagentRunId>,
}

impl StepStarted {
    /// Creates the value from its required fields (`step_name`); every optional
    /// field starts absent.
    pub fn new(step_name: impl Into<String>) -> Self {
        Self {
            step_name: step_name.into(),
            timestamp: None,
            raw_event: None,
            metadata: None,
            subagent_run_id: None,
        }
    }

    /// Sets [`timestamp`](Self::timestamp).
    #[must_use]
    pub fn with_timestamp(self, timestamp: i64) -> Self {
        Self {
            timestamp: Some(timestamp),
            ..self
        }
    }

    /// Sets [`raw_event`](Self::raw_event).
    #[must_use]
    pub fn with_raw_event(self, raw_event: Value) -> Self {
        Self {
            raw_event: crate::json::present(raw_event),
            ..self
        }
    }

    /// Sets [`metadata`](Self::metadata).
    #[must_use]
    pub fn with_metadata(self, metadata: Metadata) -> Self {
        Self {
            metadata: Some(metadata),
            ..self
        }
    }

    /// Sets [`subagent_run_id`](Self::subagent_run_id).
    #[must_use]
    pub fn with_subagent_run_id(self, subagent_run_id: impl Into<SubagentRunId>) -> Self {
        Self {
            subagent_run_id: Some(subagent_run_id.into()),
            ..self
        }
    }
}

impl From<StepStarted> for crate::event::Event {
    fn from(event: StepStarted) -> Self {
        Self::StepStarted(event)
    }
}

/// Closes a step opened by [`StepStarted`] with the same name and owner.
///
/// Schema: `$defs/StepFinishedEvent`. Spec rules: LIFE-50, LIFE-51, SUB-22.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct StepFinished {
    /// Label of the open step being closed (LIFE-50).
    pub step_name: String,
    /// Producer clock reading, conventionally milliseconds since the Unix epoch.
    ///
    /// Informational only: arrival order is the protocol order and this SDK never
    /// reorders or validates events by it (BASIC-03, BASIC-04).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timestamp: Option<i64>,
    /// The provider-native event this one was translated from, carried verbatim.
    ///
    /// Never consulted by the pipeline (BASIC-05, BASIC-06). Absent rather than
    /// `null` on the wire (BASIC-09).
    #[serde(default, skip_serializing_if = "crate::json::is_absent")]
    pub raw_event: Option<Value>,
    /// Open, vendor-prefixed annotations. Merged key by key into the item this
    /// event contributes to, where the event has one (META-06..META-15).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<Metadata>,
    /// The subagent invocation that produced this event; absent means the parent
    /// agent (SUB-01). Continuations must agree with their opener (SUB-18, SUB-24).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subagent_run_id: Option<SubagentRunId>,
}

impl StepFinished {
    /// Creates the value from its required fields (`step_name`); every optional
    /// field starts absent.
    pub fn new(step_name: impl Into<String>) -> Self {
        Self {
            step_name: step_name.into(),
            timestamp: None,
            raw_event: None,
            metadata: None,
            subagent_run_id: None,
        }
    }

    /// Sets [`timestamp`](Self::timestamp).
    #[must_use]
    pub fn with_timestamp(self, timestamp: i64) -> Self {
        Self {
            timestamp: Some(timestamp),
            ..self
        }
    }

    /// Sets [`raw_event`](Self::raw_event).
    #[must_use]
    pub fn with_raw_event(self, raw_event: Value) -> Self {
        Self {
            raw_event: crate::json::present(raw_event),
            ..self
        }
    }

    /// Sets [`metadata`](Self::metadata).
    #[must_use]
    pub fn with_metadata(self, metadata: Metadata) -> Self {
        Self {
            metadata: Some(metadata),
            ..self
        }
    }

    /// Sets [`subagent_run_id`](Self::subagent_run_id).
    #[must_use]
    pub fn with_subagent_run_id(self, subagent_run_id: impl Into<SubagentRunId>) -> Self {
        Self {
            subagent_run_id: Some(subagent_run_id.into()),
            ..self
        }
    }
}

impl From<StepFinished> for crate::event::Event {
    fn from(event: StepFinished) -> Self {
        Self::StepFinished(event)
    }
}
