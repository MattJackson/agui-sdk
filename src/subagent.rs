//! Subagents (`events/subagents.mdx`).
//!
//! Events produced by a subagent invocation carry its `subagentRunId`;
//! absence means the parent agent (SUB-01). An invocation may be announced by
//! [`SubagentStarted`] and closed by [`SubagentFinished`]/[`SubagentError`], but
//! attribution without lifecycle events is legal and accepted (SUB-11, SUB-12).
//! Ownership of each streamed entity is fixed by its opener and every
//! continuation must agree (SUB-18..SUB-26).

use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;

use crate::ids::{InterruptId, MessageId, SubagentRunId, ToolCallId};
use crate::json::{deserialize_tagged, skip_rest, Tagged};
use crate::metadata::Metadata;

/// How a subagent invocation ended (`$defs/SubagentFinishedOutcome`). Absent
/// means [`Success`](Self::Success) (SUB-16).
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "lowercase")]
#[non_exhaustive]
pub enum SubagentFinishedOutcome {
    /// The invocation completed.
    Success,
    /// The invocation paused awaiting input; terminal for this stream and
    /// continuable by a later run under the same id (SUB-04, SUB-16).
    #[serde(rename_all = "camelCase")]
    #[non_exhaustive]
    Suspended {
        /// Interrupts the invocation is waiting on (SUB-35).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        interrupt_ids: Option<Vec<InterruptId>>,
    },
}

impl SubagentFinishedOutcome {
    /// The suspended outcome for the given interrupts.
    pub fn suspended(interrupt_ids: impl IntoIterator<Item = InterruptId>) -> Self {
        SubagentFinishedOutcome::Suspended {
            interrupt_ids: Some(interrupt_ids.into_iter().collect()),
        }
    }
}

/// Wire spellings of [`SubagentFinishedOutcome`]'s `type`.
#[derive(Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum OutcomeKind {
    Success,
    Suspended,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SuspendedFields {
    #[serde(default)]
    interrupt_ids: Option<Vec<InterruptId>>,
}

impl Tagged for SubagentFinishedOutcome {
    const TAG: &'static str = "type";
    const EXPECTING: &'static str = "a subagent outcome object";
    type Tag = OutcomeKind;

    fn from_tag<'de, D: Deserializer<'de>>(tag: OutcomeKind, rest: D) -> Result<Self, D::Error> {
        Ok(match tag {
            OutcomeKind::Success => {
                skip_rest(rest)?;
                SubagentFinishedOutcome::Success
            }
            OutcomeKind::Suspended => SubagentFinishedOutcome::Suspended {
                interrupt_ids: SuspendedFields::deserialize(rest)?.interrupt_ids,
            },
        })
    }
}

impl<'de> Deserialize<'de> for SubagentFinishedOutcome {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserialize_tagged(deserializer)
    }
}

/// Announces a subagent invocation before any event attributed to it (SUB-07).
///
/// Schema: `$defs/SubagentStartedEvent`. Spec rules: SUB-06..SUB-09, SUB-27, SUB-28.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct SubagentStarted {
    /// Invocation id; never reused within a run (SUB-03, SUB-08, SUB-09).
    pub subagent_run_id: SubagentRunId,
    /// Subagent name, as listed in capabilities (CAP-06).
    pub name: String,
    /// Human-readable description of this invocation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Spawning invocation, when a subagent delegates (SUB-27, SUB-28).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_subagent_run_id: Option<SubagentRunId>,
    /// Tool call that spawned the invocation, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_tool_call_id: Option<ToolCallId>,
    /// Message that spawned the invocation, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_message_id: Option<MessageId>,
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

impl SubagentStarted {
    /// Creates the value from its required fields (`subagent_run_id`, `name`); every optional
    /// field starts absent.
    pub fn new(subagent_run_id: impl Into<SubagentRunId>, name: impl Into<String>) -> Self {
        Self {
            subagent_run_id: subagent_run_id.into(),
            name: name.into(),
            description: None,
            parent_subagent_run_id: None,
            parent_tool_call_id: None,
            parent_message_id: None,
            timestamp: None,
            raw_event: None,
            metadata: None,
        }
    }

    /// Sets [`description`](Self::description).
    #[must_use]
    pub fn with_description(self, description: impl Into<String>) -> Self {
        Self {
            description: Some(description.into()),
            ..self
        }
    }

    /// Sets [`parent_subagent_run_id`](Self::parent_subagent_run_id).
    #[must_use]
    pub fn with_parent_subagent_run_id(
        self,
        parent_subagent_run_id: impl Into<SubagentRunId>,
    ) -> Self {
        Self {
            parent_subagent_run_id: Some(parent_subagent_run_id.into()),
            ..self
        }
    }

    /// Sets [`parent_tool_call_id`](Self::parent_tool_call_id).
    #[must_use]
    pub fn with_parent_tool_call_id(self, parent_tool_call_id: impl Into<ToolCallId>) -> Self {
        Self {
            parent_tool_call_id: Some(parent_tool_call_id.into()),
            ..self
        }
    }

    /// Sets [`parent_message_id`](Self::parent_message_id).
    #[must_use]
    pub fn with_parent_message_id(self, parent_message_id: impl Into<MessageId>) -> Self {
        Self {
            parent_message_id: Some(parent_message_id.into()),
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

impl From<SubagentStarted> for crate::event::Event {
    fn from(event: SubagentStarted) -> Self {
        Self::SubagentStarted(event)
    }
}

/// Ends an invocation normally. Absent outcome means success (SUB-16).
///
/// Schema: `$defs/SubagentFinishedEvent`. Spec rules: SUB-10, SUB-14..SUB-17, SUB-36.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct SubagentFinished {
    /// The invocation being closed (SUB-06, SUB-36).
    pub subagent_run_id: SubagentRunId,
    /// The invocation's return value, if any.
    #[serde(default, skip_serializing_if = "crate::json::is_absent")]
    pub result: Option<Value>,
    /// Success or suspended (SUB-16, SUB-17).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outcome: Option<SubagentFinishedOutcome>,
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

impl SubagentFinished {
    /// Creates the value from its required fields (`subagent_run_id`); every optional
    /// field starts absent.
    pub fn new(subagent_run_id: impl Into<SubagentRunId>) -> Self {
        Self {
            subagent_run_id: subagent_run_id.into(),
            result: None,
            outcome: None,
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
    pub fn with_outcome(self, outcome: SubagentFinishedOutcome) -> Self {
        Self {
            outcome: Some(outcome),
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

impl From<SubagentFinished> for crate::event::Event {
    fn from(event: SubagentFinished) -> Self {
        Self::SubagentFinished(event)
    }
}

/// Ends an invocation in failure. Ends the invocation, not the run (SUB-14).
///
/// Schema: `$defs/SubagentErrorEvent`. Spec rules: SUB-10, SUB-14.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct SubagentError {
    /// The invocation that failed (SUB-06).
    pub subagent_run_id: SubagentRunId,
    /// Human-readable failure description.
    pub message: String,
    /// Open, producer-defined failure code.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
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

impl SubagentError {
    /// Creates the value from its required fields (`subagent_run_id`, `message`); every optional
    /// field starts absent.
    pub fn new(subagent_run_id: impl Into<SubagentRunId>, message: impl Into<String>) -> Self {
        Self {
            subagent_run_id: subagent_run_id.into(),
            message: message.into(),
            code: None,
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

impl From<SubagentError> for crate::event::Event {
    fn from(event: SubagentError) -> Self {
        Self::SubagentError(event)
    }
}
