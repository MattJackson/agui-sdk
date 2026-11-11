//! Activity messages (`events/activity.mdx`).
//!
//! Activities are client-only structured messages (progress, plans, cards)
//! created by [`ActivitySnapshot`] and patched by [`ActivityDelta`]. Their
//! history entry is an [`ActivityMessage`](crate::message::ActivityMessage),
//! which the outgoing pipeline strips from run input (RUNIN-12, ACT-18).

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::ids::{MessageId, SubagentRunId};
use crate::json::JsonObject;
use crate::metadata::Metadata;
use crate::patch::PatchOperation;

/// Creates an activity message, or replaces one unless `replace` is `false`
/// (ACT-02..ACT-07).
///
/// Schema: `$defs/ActivitySnapshotEvent`. Spec rules: ACT-01..ACT-08, META-12.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct ActivitySnapshot {
    /// Activity message to create or replace.
    pub message_id: MessageId,
    /// Open activity kind; unknown values are tolerated (ACT-01).
    pub activity_type: String,
    /// Activity payload; always a JSON object (ACT-15).
    pub content: JsonObject,
    /// `false` leaves an existing message as it stands (ACT-04); absent means `true`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replace: Option<bool>,
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

impl ActivitySnapshot {
    /// Creates the value from its required fields (`message_id`, `activity_type`, `content`); every optional
    /// field starts absent.
    pub fn new(
        message_id: impl Into<MessageId>,
        activity_type: impl Into<String>,
        content: JsonObject,
    ) -> Self {
        Self {
            message_id: message_id.into(),
            activity_type: activity_type.into(),
            content,
            replace: None,
            timestamp: None,
            raw_event: None,
            metadata: None,
            subagent_run_id: None,
        }
    }

    /// Sets [`replace`](Self::replace).
    #[must_use]
    pub fn with_replace(self, replace: bool) -> Self {
        Self {
            replace: Some(replace),
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

impl From<ActivitySnapshot> for crate::event::Event {
    fn from(event: ActivitySnapshot) -> Self {
        Self::ActivitySnapshot(event)
    }
}

/// Patches an existing activity message's content. A delta naming a missing
/// or non-activity message is skipped with a warning (ACT-12..ACT-14).
///
/// Schema: `$defs/ActivityDeltaEvent`. Spec rules: ACT-09..ACT-16.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct ActivityDelta {
    /// Activity message to patch.
    pub message_id: MessageId,
    /// Replaces the message's activity type (ACT-09, ACT-10).
    pub activity_type: String,
    /// RFC 6902 operations applied atomically to `content` (ACT-15, ACT-16).
    pub patch: Vec<PatchOperation>,
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

impl ActivityDelta {
    /// Creates the value from its required fields (`message_id`, `activity_type`, `patch`); every optional
    /// field starts absent.
    pub fn new(
        message_id: impl Into<MessageId>,
        activity_type: impl Into<String>,
        patch: Vec<PatchOperation>,
    ) -> Self {
        Self {
            message_id: message_id.into(),
            activity_type: activity_type.into(),
            patch,
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

impl From<ActivityDelta> for crate::event::Event {
    fn from(event: ActivityDelta) -> Self {
        Self::ActivityDelta(event)
    }
}
