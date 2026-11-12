//! RAW and CUSTOM (`events/passthrough.mdx`).
//!
//! Both are standalone: they open and close no items of their own and may
//! appear anywhere in an open run (PASS-01); CUSTOM, unlike RAW, ends an open
//! chunk stream in its lane. Their payloads are untrusted; this SDK never derives
//! protocol behaviour from them (PASS-03, PASS-09).

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::ids::SubagentRunId;
use crate::metadata::Metadata;

/// A provider-native event passed through for observability. Never drives
/// protocol behaviour (PASS-03) and is untrusted (PASS-09, PASS-10).
///
/// Schema: `$defs/RawEvent`. Spec rules: PASS-01..PASS-04, PASS-09.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct Raw {
    /// The native event, any JSON value (PASS-02).
    pub event: Value,
    /// Where the native event came from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
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

impl Raw {
    /// Creates the value from its required fields (`event`); every optional
    /// field starts absent.
    #[must_use]
    pub fn new(event: Value) -> Self {
        Self {
            event,
            source: None,
            timestamp: None,
            raw_event: None,
            metadata: None,
            subagent_run_id: None,
        }
    }

    /// Sets [`source`](Self::source).
    #[must_use]
    pub fn with_source(self, source: impl Into<String>) -> Self {
        Self {
            source: Some(source.into()),
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

impl From<Raw> for crate::event::Event {
    fn from(event: Raw) -> Self {
        Self::Raw(event)
    }
}

/// An application-defined event. Unknown names are ignored without warning
/// (PASS-06). Ends an open chunk stream in its lane (PASS-01).
///
/// Schema: `$defs/CustomEvent`. Spec rules: PASS-01, PASS-05..PASS-09.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct Custom {
    /// Event name; invented names should be vendor-prefixed (PASS-07).
    pub name: String,
    /// Payload, any JSON value including `null` (BASIC-10). Untrusted (PASS-09).
    pub value: Value,
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

impl Custom {
    /// Creates the value from its required fields (`name`, `value`); every optional
    /// field starts absent.
    pub fn new(name: impl Into<String>, value: Value) -> Self {
        Self {
            name: name.into(),
            value,
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

impl From<Custom> for crate::event::Event {
    fn from(event: Custom) -> Self {
        Self::Custom(event)
    }
}
