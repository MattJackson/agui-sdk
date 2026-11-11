//! Text messages (`events/text-messages.mdx`).
//!
//! A message streams as start, content, end, matched by `messageId`, or as
//! [`TextMessageChunk`] shorthand that the pipeline expands first (TEXT-02,
//! TEXT-09). A closed message may be reopened by a new start, which must agree
//! with it; on a disagreement the established values stand and the client
//! warns (TEXT-05..TEXT-07).

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::ids::{MessageId, SubagentRunId};
use crate::metadata::Metadata;

/// Author role of a streamed text message (`$defs/TextMessageRole`).
/// Absent on the wire means [`Assistant`](Self::Assistant).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
#[non_exhaustive]
pub enum TextMessageRole {
    /// Developer instructions.
    Developer,
    /// System prompt.
    System,
    /// The agent. The default.
    #[default]
    Assistant,
    /// The end user.
    User,
}

/// Opens (or reopens) a streamed text message.
///
/// Schema: `$defs/TextMessageStartEvent`. Spec rules: STRM-01, TEXT-03, TEXT-05, TEXT-06.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct TextMessageStart {
    /// Identifier of the message being opened. Must not already be open (STRM-01).
    pub message_id: MessageId,
    /// Author role; absent means [`TextMessageRole::Assistant`] (TEXT-03).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<TextMessageRole>,
    /// Optional author display name (TEXT-03).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
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

impl TextMessageStart {
    /// Creates the value from its required fields (`message_id`); every optional
    /// field starts absent.
    pub fn new(message_id: impl Into<MessageId>) -> Self {
        Self {
            message_id: message_id.into(),
            role: None,
            name: None,
            timestamp: None,
            raw_event: None,
            metadata: None,
            subagent_run_id: None,
        }
    }

    /// Sets [`role`](Self::role).
    #[must_use]
    pub fn with_role(self, role: TextMessageRole) -> Self {
        Self {
            role: Some(role),
            ..self
        }
    }

    /// Sets [`name`](Self::name).
    #[must_use]
    pub fn with_name(self, name: impl Into<String>) -> Self {
        Self {
            name: Some(name.into()),
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

impl From<TextMessageStart> for crate::event::Event {
    fn from(event: TextMessageStart) -> Self {
        Self::TextMessageStart(event)
    }
}

/// Appends text to an open message. Deltas concatenate in arrival order (TEXT-04).
///
/// Schema: `$defs/TextMessageContentEvent`. Spec rules: STRM-02, STRM-04, TEXT-04.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct TextMessageContent {
    /// Open message this delta belongs to (STRM-02).
    pub message_id: MessageId,
    /// Text fragment appended to the message.
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

impl TextMessageContent {
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

impl From<TextMessageContent> for crate::event::Event {
    fn from(event: TextMessageContent) -> Self {
        Self::TextMessageContent(event)
    }
}

/// Closes an open text message.
///
/// Schema: `$defs/TextMessageEndEvent`. Spec rules: STRM-02, STRM-03.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct TextMessageEnd {
    /// Open message being closed (STRM-02).
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

impl TextMessageEnd {
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

impl From<TextMessageEnd> for crate::event::Event {
    fn from(event: TextMessageEnd) -> Self {
        Self::TextMessageEnd(event)
    }
}

/// Shorthand that the client pipeline expands into start/content/end before
/// verification (STRM-07, TEXT-09). The first chunk of a message must carry
/// `message_id` (STRM-11); later ones may omit opener fields (STRM-15) and a
/// conflicting repeat is fatal (STRM-20).
///
/// Schema: `$defs/TextMessageChunkEvent`. Spec rules: STRM-07..STRM-27, TEXT-09.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct TextMessageChunk {
    /// Message identifier; required on the first chunk of a message (STRM-11).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message_id: Option<MessageId>,
    /// Author role on the first chunk; absent establishes `assistant` (STRM-12).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<TextMessageRole>,
    /// Text fragment, if any. A metadata-only chunk is legal (STRM-27).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delta: Option<String>,
    /// Optional author display name; repeats must agree (STRM-19).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
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

impl TextMessageChunk {
    /// Creates the value with every field absent.
    #[must_use]
    pub fn new() -> Self {
        Self {
            message_id: None,
            role: None,
            delta: None,
            name: None,
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

    /// Sets [`role`](Self::role).
    #[must_use]
    pub fn with_role(self, role: TextMessageRole) -> Self {
        Self {
            role: Some(role),
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

    /// Sets [`name`](Self::name).
    #[must_use]
    pub fn with_name(self, name: impl Into<String>) -> Self {
        Self {
            name: Some(name.into()),
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

impl From<TextMessageChunk> for crate::event::Event {
    fn from(event: TextMessageChunk) -> Self {
        Self::TextMessageChunk(event)
    }
}
