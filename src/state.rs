//! State and message snapshots (`events/state.mdx`, `patterns/snapshots.mdx`).
//!
//! State is any JSON value (STATE-03). [`StateSnapshot`] replaces it,
//! [`StateDelta`] patches it atomically with [`crate::patch`], and a well-formed
//! patch that fails to apply is skipped with a warning while a malformed one is
//! fatal (SNAP-09..SNAP-14). State persists across runs on a thread and
//! subagents share the run's single state (STATE-10, SUB-31).
//!
//! [`MessagesSnapshot`] reconciles history by message id (STATE-04..STATE-08).
//! On the server, [`StatePublisher`][crate::server::state::StatePublisher]
//! chooses between snapshot and delta for the agent.
#![cfg_attr(
    not(feature = "server"),
    doc = "",
    doc = "[crate::server::state::StatePublisher]: https://docs.rs/agui-sdk/latest/agui_sdk/server/state/struct.StatePublisher.html"
)]

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::ids::SubagentRunId;
use crate::message::Message;
use crate::metadata::Metadata;
use crate::patch::PatchOperation;

/// Replaces the run's state wholesale; never merged (STATE-01, SNAP-01).
///
/// Schema: `$defs/StateSnapshotEvent`. Spec rules: SNAP-01, SNAP-02, SNAP-16, STATE-01, STATE-03, SUB-31.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct StateSnapshot {
    /// The new state. Any JSON value, `null` included (BASIC-10, STATE-03).
    pub snapshot: Value,
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

impl StateSnapshot {
    /// Creates the value from its required fields (`snapshot`); every optional
    /// field starts absent.
    #[must_use]
    pub fn new(snapshot: Value) -> Self {
        Self {
            snapshot,
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

impl From<StateSnapshot> for crate::event::Event {
    fn from(event: StateSnapshot) -> Self {
        Self::StateSnapshot(event)
    }
}

/// Amends state with an RFC 6902 patch, applied atomically (SNAP-05, SNAP-11).
///
/// Schema: `$defs/StateDeltaEvent`. Spec rules: SNAP-05..SNAP-14, STATE-02, STATE-11.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct StateDelta {
    /// Operations, applied in order. Operation objects are open: unknown
    /// members survive enforcement (PROC-15, SNAP-08).
    pub delta: Vec<PatchOperation>,
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

impl StateDelta {
    /// Creates the value from its required fields (`delta`); every optional
    /// field starts absent.
    #[must_use]
    pub fn new(delta: Vec<PatchOperation>) -> Self {
        Self {
            delta,
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

impl From<StateDelta> for crate::event::Event {
    fn from(event: StateDelta) -> Self {
        Self::StateDelta(event)
    }
}

/// Reconciles the client's message history by id (STATE-04..STATE-08).
/// Carries no attribution of its own (STATE-09).
///
/// Schema: `$defs/MessagesSnapshotEvent`. Spec rules: STATE-04..STATE-09, STATE-14, LIFE-46.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct MessagesSnapshot {
    /// The complete set of messages the producer owns, in order (STATE-14).
    pub messages: Vec<Message>,
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

impl MessagesSnapshot {
    /// Creates the value from its required fields (`messages`); every optional
    /// field starts absent.
    #[must_use]
    pub fn new(messages: Vec<Message>) -> Self {
        Self {
            messages,
            timestamp: None,
            raw_event: None,
            metadata: None,
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

impl From<MessagesSnapshot> for crate::event::Event {
    fn from(event: MessagesSnapshot) -> Self {
        Self::MessagesSnapshot(event)
    }
}
