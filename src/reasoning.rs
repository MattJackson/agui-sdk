//! Reasoning (`events/reasoning.mdx`).
//!
//! Spans ([`ReasoningStart`]/[`ReasoningEnd`]) group reasoning messages
//! ([`ReasoningMessageStart`] and friends); both follow the streaming
//! discipline and a violation fails the run (REAS-08). Encrypted values are
//! opaque: stored on the message or tool call they name and returned unchanged
//! in later input, never parsed (REAS-14, REAS-15). An unknown `entityId`
//! warns and is dropped, never fatal (REAS-20..REAS-22).

use std::fmt;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::ids::{MessageId, SubagentRunId};
use crate::metadata::Metadata;

/// The fixed `role` of a reasoning message: `reasoning` (REAS-06).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
#[non_exhaustive]
pub enum ReasoningRole {
    /// `reasoning`.
    #[default]
    Reasoning,
}

/// What a [`ReasoningEncryptedValue`]'s `entityId` names
/// (`$defs/ReasoningEncryptedValueSubtype`). A string the schema does not name
/// is stripped by enforcement (REAS-23).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
#[non_exhaustive]
pub enum EncryptedValueSubtype {
    /// A tool call id.
    ToolCall,
    /// A message id.
    Message,
}

/// Opens a reasoning span. The span id namespaces nothing (REAS-05, REAS-12).
///
/// Schema: `$defs/ReasoningStartEvent`. Spec rules: REAS-02, REAS-05, REAS-10, REAS-12.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct ReasoningStart {
    /// Span identifier; must not already be open as a span (REAS-02).
    pub message_id: MessageId,
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

impl ReasoningStart {
    /// Creates the value from its required fields (`message_id`); every optional
    /// field starts absent.
    pub fn new(message_id: impl Into<MessageId>) -> Self {
        Self {
            message_id: message_id.into(),
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

impl From<ReasoningStart> for crate::event::Event {
    fn from(event: ReasoningStart) -> Self {
        Self::ReasoningStart(event)
    }
}

/// Opens a reasoning message.
///
/// Schema: `$defs/ReasoningMessageStartEvent`. Spec rules: REAS-06, REAS-11.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct ReasoningMessageStart {
    /// Reasoning message identifier (REAS-11).
    pub message_id: MessageId,
    /// Fixed `reasoning`; any other value is malformed (REAS-06).
    pub role: ReasoningRole,
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

impl ReasoningMessageStart {
    /// Creates the value from its required fields (`message_id`, `role`); every optional
    /// field starts absent.
    pub fn new(message_id: impl Into<MessageId>, role: ReasoningRole) -> Self {
        Self {
            message_id: message_id.into(),
            role,
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

impl From<ReasoningMessageStart> for crate::event::Event {
    fn from(event: ReasoningMessageStart) -> Self {
        Self::ReasoningMessageStart(event)
    }
}

/// Appends text to an open reasoning message.
///
/// Schema: `$defs/ReasoningMessageContentEvent`. Spec rules: REAS-08, REAS-09.
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct ReasoningMessageContent {
    /// Open reasoning message (REAS-09).
    pub message_id: MessageId,
    /// Reasoning text fragment.
    pub delta: String,
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

impl ReasoningMessageContent {
    /// Creates the value from its required fields (`message_id`, `delta`); every optional
    /// field starts absent.
    pub fn new(message_id: impl Into<MessageId>, delta: impl Into<String>) -> Self {
        Self {
            message_id: message_id.into(),
            delta: delta.into(),
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

impl From<ReasoningMessageContent> for crate::event::Event {
    fn from(event: ReasoningMessageContent) -> Self {
        Self::ReasoningMessageContent(event)
    }
}

/// Closes an open reasoning message.
///
/// Schema: `$defs/ReasoningMessageEndEvent`. Spec rules: REAS-08, REAS-09.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct ReasoningMessageEnd {
    /// Open reasoning message being closed (REAS-09).
    pub message_id: MessageId,
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

impl ReasoningMessageEnd {
    /// Creates the value from its required fields (`message_id`); every optional
    /// field starts absent.
    pub fn new(message_id: impl Into<MessageId>) -> Self {
        Self {
            message_id: message_id.into(),
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

impl From<ReasoningMessageEnd> for crate::event::Event {
    fn from(event: ReasoningMessageEnd) -> Self {
        Self::ReasoningMessageEnd(event)
    }
}

/// Shorthand expanded into reasoning start/content/end; the first chunk
/// carries `message_id` (STRM-13, REAS-07).
///
/// Schema: `$defs/ReasoningMessageChunkEvent`. Spec rules: STRM-13, REAS-07.
#[derive(Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct ReasoningMessageChunk {
    /// Required on the first chunk (STRM-13).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message_id: Option<MessageId>,
    /// Reasoning text fragment, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delta: Option<String>,
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

impl ReasoningMessageChunk {
    /// Creates the value with every field absent.
    #[must_use]
    pub fn new() -> Self {
        Self {
            message_id: None,
            delta: None,
            timestamp: None,
            raw_event: None,
            metadata: None,
            subagent_run_id: None,
        }
    }

    /// Sets [`message_id`](Self::message_id).
    #[must_use]
    pub fn with_message_id(self, message_id: impl Into<MessageId>) -> Self {
        Self {
            message_id: Some(message_id.into()),
            ..self
        }
    }

    /// Sets [`delta`](Self::delta).
    #[must_use]
    pub fn with_delta(self, delta: impl Into<String>) -> Self {
        Self {
            delta: Some(delta.into()),
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

    /// Sets [`subagent_run_id`](Self::subagent_run_id).
    #[must_use]
    pub fn with_subagent_run_id(self, subagent_run_id: impl Into<SubagentRunId>) -> Self {
        Self {
            subagent_run_id: Some(subagent_run_id.into()),
            ..self
        }
    }
}

impl From<ReasoningMessageChunk> for crate::event::Event {
    fn from(event: ReasoningMessageChunk) -> Self {
        Self::ReasoningMessageChunk(event)
    }
}

/// Closes a reasoning span.
///
/// Schema: `$defs/ReasoningEndEvent`. Spec rules: REAS-03, REAS-04, REAS-10.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct ReasoningEnd {
    /// Open span being closed (REAS-04, REAS-10).
    pub message_id: MessageId,
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

impl ReasoningEnd {
    /// Creates the value from its required fields (`message_id`); every optional
    /// field starts absent.
    pub fn new(message_id: impl Into<MessageId>) -> Self {
        Self {
            message_id: message_id.into(),
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

impl From<ReasoningEnd> for crate::event::Event {
    fn from(event: ReasoningEnd) -> Self {
        Self::ReasoningEnd(event)
    }
}

/// Attaches an opaque provider artefact to a message or tool call. Standalone:
/// opens and closes nothing (REAS-19).
///
/// Schema: `$defs/ReasoningEncryptedValueEvent`. Spec rules: REAS-14..REAS-23.
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct ReasoningEncryptedValue {
    /// Which kind of entity `entity_id` names.
    pub subtype: EncryptedValueSubtype,
    /// Message or tool-call id. Naming nothing known warns, never fails (REAS-20..REAS-22).
    pub entity_id: String,
    /// Opaque bytes-as-text; never parsed (REAS-14).
    pub encrypted_value: String,
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

impl ReasoningEncryptedValue {
    /// Creates the value from its required fields (`subtype`, `entity_id`, `encrypted_value`); every optional
    /// field starts absent.
    pub fn new(
        subtype: EncryptedValueSubtype,
        entity_id: impl Into<String>,
        encrypted_value: impl Into<String>,
    ) -> Self {
        Self {
            subtype,
            entity_id: entity_id.into(),
            encrypted_value: encrypted_value.into(),
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

impl From<ReasoningEncryptedValue> for crate::event::Event {
    fn from(event: ReasoningEncryptedValue) -> Self {
        Self::ReasoningEncryptedValue(event)
    }
}

/// Stands in for a value `Debug` must not print: encrypted values and
/// reasoning text are provider artefacts that can carry model internals, and
/// debug output tends to reach logs.
pub(crate) struct Redacted;

impl fmt::Debug for Redacted {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("<redacted>")
    }
}

/// `Some(<redacted>)` or `None`, so `Debug` still shows presence.
pub(crate) fn redact<T>(value: &Option<T>) -> Option<Redacted> {
    value.as_ref().map(|_| Redacted)
}

impl fmt::Debug for ReasoningMessageContent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ReasoningMessageContent")
            .field("message_id", &self.message_id)
            .field("delta", &Redacted)
            .field("timestamp", &self.timestamp)
            .field("raw_event", &redact(&self.raw_event))
            .field("metadata", &self.metadata)
            .field("subagent_run_id", &self.subagent_run_id)
            .finish()
    }
}

impl fmt::Debug for ReasoningMessageChunk {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ReasoningMessageChunk")
            .field("message_id", &self.message_id)
            .field("delta", &redact(&self.delta))
            .field("timestamp", &self.timestamp)
            .field("raw_event", &redact(&self.raw_event))
            .field("metadata", &self.metadata)
            .field("subagent_run_id", &self.subagent_run_id)
            .finish()
    }
}

impl fmt::Debug for ReasoningEncryptedValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ReasoningEncryptedValue")
            .field("subtype", &self.subtype)
            .field("entity_id", &self.entity_id)
            .field("encrypted_value", &Redacted)
            .field("timestamp", &self.timestamp)
            .field("raw_event", &redact(&self.raw_event))
            .field("metadata", &self.metadata)
            .field("subagent_run_id", &self.subagent_run_id)
            .finish()
    }
}
