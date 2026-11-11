//! Tool calls (`events/tool-calls.mdx`) and tool definitions.
//!
//! A call streams as start, args, end (or [`ToolCallChunk`] shorthand) and is
//! answered either in-stream by [`ToolCallResult`] (agent-executed) or by the
//! client in the next run's messages (frontend tools, TOOL-12, TOOL-33).
//! Reopening a closed call appends arguments, per the spec (TOOL-06,
//! TOOL-43); the latest name wins with a warning.
//!
//! Arguments are untrusted text. Clients execute frontend calls only through
//! [`Thread::approve`][crate::client::Thread::approve], which records who
//! approved, and validate them with
//! [`ApprovedToolCall::check_args_subset`][crate::client::ApprovedToolCall::check_args_subset]
//! (TOOL-38..TOOL-40).
#![cfg_attr(
    not(feature = "client"),
    doc = "",
    doc = "[crate::client::Thread::approve]: https://docs.rs/agui-sdk/latest/agui_sdk/client/struct.Thread.html#method.approve"
)]
#![cfg_attr(
    not(feature = "client"),
    doc = "",
    doc = "[crate::client::ApprovedToolCall::check_args_subset]: https://docs.rs/agui-sdk/latest/agui_sdk/client/struct.ApprovedToolCall.html#method.check_args_subset"
)]

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::content::Content;
use crate::ids::{MessageId, SubagentRunId, ToolCallId};
use crate::metadata::Metadata;

/// The fixed `role` of a [`ToolCallResult`]: `tool`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
#[non_exhaustive]
pub enum ToolRole {
    /// `tool`.
    #[default]
    Tool,
}

/// The fixed `type` of a [`ToolCall`]: `function`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
#[non_exhaustive]
pub enum ToolCallType {
    /// `function`.
    #[default]
    Function,
}

/// Opens (or reopens) a tool call.
///
/// Schema: `$defs/ToolCallStartEvent`. Spec rules: STRM-01, TOOL-01, TOOL-02, TOOL-06, TOOL-07.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct ToolCallStart {
    /// Identifier of the call. Must not already be open (STRM-01).
    pub tool_call_id: ToolCallId,
    /// Name of the tool being called.
    pub tool_call_name: String,
    /// Assistant message that carries the call (TOOL-01, SUB-26).
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
    /// The subagent invocation that produced this event; absent means the parent
    /// agent (SUB-01). Continuations must agree with their opener (SUB-18, SUB-24).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subagent_run_id: Option<SubagentRunId>,
}

impl ToolCallStart {
    /// Creates the value from its required fields (`tool_call_id`, `tool_call_name`); every optional
    /// field starts absent.
    pub fn new(tool_call_id: impl Into<ToolCallId>, tool_call_name: impl Into<String>) -> Self {
        Self {
            tool_call_id: tool_call_id.into(),
            tool_call_name: tool_call_name.into(),
            parent_message_id: None,
            timestamp: None,
            raw_event: None,
            metadata: None,
            subagent_run_id: None,
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

    /// Sets [`subagent_run_id`](Self::subagent_run_id).
    #[must_use]
    pub fn with_subagent_run_id(self, subagent_run_id: impl Into<SubagentRunId>) -> Self {
        Self {
            subagent_run_id: Some(subagent_run_id.into()),
            ..self
        }
    }
}

impl From<ToolCallStart> for crate::event::Event {
    fn from(event: ToolCallStart) -> Self {
        Self::ToolCallStart(event)
    }
}

/// Appends argument text to an open call. The protocol does not validate the
/// concatenated text (TOOL-03); see
/// [`ApprovedToolCall::check_args_subset`][crate::client::ApprovedToolCall::check_args_subset].
///
/// Schema: `$defs/ToolCallArgsEvent`. Spec rules: STRM-02, TOOL-03, TOOL-43.
#[cfg_attr(
    not(feature = "client"),
    doc = "",
    doc = "[crate::client::ApprovedToolCall::check_args_subset]: https://docs.rs/agui-sdk/latest/agui_sdk/client/struct.ApprovedToolCall.html#method.check_args_subset"
)]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct ToolCallArgs {
    /// Open call these arguments belong to (STRM-02).
    pub tool_call_id: ToolCallId,
    /// Argument text fragment, appended in arrival order (TOOL-03, TOOL-43).
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

impl ToolCallArgs {
    /// Creates the value from its required fields (`tool_call_id`, `delta`); every optional
    /// field starts absent.
    pub fn new(tool_call_id: impl Into<ToolCallId>, delta: impl Into<String>) -> Self {
        Self {
            tool_call_id: tool_call_id.into(),
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

impl From<ToolCallArgs> for crate::event::Event {
    fn from(event: ToolCallArgs) -> Self {
        Self::ToolCallArgs(event)
    }
}

/// Marks the call's arguments complete. Completeness, not validity (TOOL-05);
/// no client acts on arguments before this event (TOOL-04).
///
/// Schema: `$defs/ToolCallEndEvent`. Spec rules: STRM-02, TOOL-04, TOOL-05.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct ToolCallEnd {
    /// Open call being closed (STRM-02).
    pub tool_call_id: ToolCallId,
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

impl ToolCallEnd {
    /// Creates the value from its required fields (`tool_call_id`); every optional
    /// field starts absent.
    pub fn new(tool_call_id: impl Into<ToolCallId>) -> Self {
        Self {
            tool_call_id: tool_call_id.into(),
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

impl From<ToolCallEnd> for crate::event::Event {
    fn from(event: ToolCallEnd) -> Self {
        Self::ToolCallEnd(event)
    }
}

/// Shorthand expanded by the pipeline into start/args/end. The first chunk
/// carries both `tool_call_id` and `tool_call_name` (STRM-14, TOOL-09).
///
/// Schema: `$defs/ToolCallChunkEvent`. Spec rules: STRM-14, STRM-19, STRM-20, TOOL-09, TOOL-10.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct ToolCallChunk {
    /// Call identifier; required on the first chunk (STRM-14).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<ToolCallId>,
    /// Tool name; required on the first chunk, repeats must agree (TOOL-10).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_name: Option<String>,
    /// Carrying assistant message; repeats must agree (TOOL-10).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_message_id: Option<MessageId>,
    /// Argument text fragment, if any.
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

impl ToolCallChunk {
    /// Creates the value with every field absent.
    #[must_use]
    pub fn new() -> Self {
        Self {
            tool_call_id: None,
            tool_call_name: None,
            parent_message_id: None,
            delta: None,
            timestamp: None,
            raw_event: None,
            metadata: None,
            subagent_run_id: None,
        }
    }

    /// Sets [`tool_call_id`](Self::tool_call_id).
    #[must_use]
    pub fn with_tool_call_id(self, tool_call_id: impl Into<ToolCallId>) -> Self {
        Self {
            tool_call_id: Some(tool_call_id.into()),
            ..self
        }
    }

    /// Sets [`tool_call_name`](Self::tool_call_name).
    #[must_use]
    pub fn with_tool_call_name(self, tool_call_name: impl Into<String>) -> Self {
        Self {
            tool_call_name: Some(tool_call_name.into()),
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

impl From<ToolCallChunk> for crate::event::Event {
    fn from(event: ToolCallChunk) -> Self {
        Self::ToolCallChunk(event)
    }
}

/// Answers a call with a new tool message; it does not reopen the call
/// (TOOL-11). Never emitted for a frontend tool (TOOL-24).
///
/// Schema: `$defs/ToolCallResultEvent`. Spec rules: TOOL-11..TOOL-19, TOOL-24.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct ToolCallResult {
    /// Identifier of the tool message this result mints (TOOL-11).
    pub message_id: MessageId,
    /// The call being answered.
    pub tool_call_id: ToolCallId,
    /// A string or an ordered list of parts (TOOL-13); copied unchanged into
    /// the minted tool message (TOOL-14).
    pub content: Content,
    /// Fixed `tool` when present.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<ToolRole>,
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

impl ToolCallResult {
    /// Creates the value from its required fields (`message_id`, `tool_call_id`, `content`); every optional
    /// field starts absent.
    pub fn new(
        message_id: impl Into<MessageId>,
        tool_call_id: impl Into<ToolCallId>,
        content: Content,
    ) -> Self {
        Self {
            message_id: message_id.into(),
            tool_call_id: tool_call_id.into(),
            content,
            role: None,
            timestamp: None,
            raw_event: None,
            metadata: None,
            subagent_run_id: None,
        }
    }

    /// Sets [`role`](Self::role).
    #[must_use]
    pub fn with_role(self, role: ToolRole) -> Self {
        Self {
            role: Some(role),
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

impl From<ToolCallResult> for crate::event::Event {
    fn from(event: ToolCallResult) -> Self {
        Self::ToolCallResult(event)
    }
}

/// A tool definition: a frontend tool advertised in
/// [`RunAgentInput::tools`](crate::run_input::RunAgentInput::tools), or one of the agent's own
/// tools in [`ToolsCapabilities::items`](crate::capability::ToolsCapabilities::items). The two
/// lists never merge (CAP-05).
///
/// Schema: `$defs/Tool`. Spec rules: RUNIN-19, CAP-05, TOOL-35, TOOL-38.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct Tool {
    /// Tool name, as the model will call it.
    pub name: String,
    /// What the tool does, for the model.
    pub description: String,
    /// JSON Schema of the arguments. Used by
    /// [`ApprovedToolCall::check_args_subset`][crate::client::ApprovedToolCall::check_args_subset] (TOOL-38).
    #[cfg_attr(
        not(feature = "client"),
        doc = "",
        doc = "[crate::client::ApprovedToolCall::check_args_subset]: https://docs.rs/agui-sdk/latest/agui_sdk/client/struct.ApprovedToolCall.html#method.check_args_subset"
    )]
    #[serde(default, skip_serializing_if = "crate::json::is_absent")]
    pub parameters: Option<Value>,
    /// Stored annotations; plain data, never merged into (META-16).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<Metadata>,
}

impl Tool {
    /// Creates the value from its required fields (`name`, `description`); every optional
    /// field starts absent.
    pub fn new(name: impl Into<String>, description: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            parameters: None,
            metadata: None,
        }
    }

    /// Sets [`parameters`](Self::parameters).
    #[must_use]
    pub fn with_parameters(self, parameters: Value) -> Self {
        Self {
            parameters: crate::json::present(parameters),
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

/// A tool call as stored on an [`AssistantMessage`](crate::message::AssistantMessage).
///
/// Schema: `$defs/ToolCall`. Spec rules: META-10, REAS-15.
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct ToolCall {
    /// Call identifier, as carried by `TOOL_CALL_START`.
    pub id: ToolCallId,
    /// Fixed `function`.
    #[serde(rename = "type")]
    pub kind: ToolCallType,
    /// Tool name and accumulated argument text.
    pub function: FunctionCall,
    /// Opaque provider artefact; stored and returned unmodified (REAS-14, REAS-15).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub encrypted_value: Option<String>,
    /// Merged from the call's start/args/end events, not the owning message (META-10).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<Metadata>,
}

impl ToolCall {
    /// Creates the value from its required fields (`id`, `kind`, `function`); every optional
    /// field starts absent.
    pub fn new(id: impl Into<ToolCallId>, kind: ToolCallType, function: FunctionCall) -> Self {
        Self {
            id: id.into(),
            kind,
            function,
            encrypted_value: None,
            metadata: None,
        }
    }

    /// Sets [`encrypted_value`](Self::encrypted_value).
    #[must_use]
    pub fn with_encrypted_value(self, encrypted_value: impl Into<String>) -> Self {
        Self {
            encrypted_value: Some(encrypted_value.into()),
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

/// Name and argument text of a [`ToolCall`].
///
/// Schema: `$defs/FunctionCall`. Spec rules: TOOL-03.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct FunctionCall {
    /// Tool name.
    pub name: String,
    /// Concatenated argument text; conventionally JSON, never validated by the protocol (TOOL-03).
    pub arguments: String,
}

impl FunctionCall {
    /// Creates the value from its required fields (`name`, `arguments`).
    pub fn new(name: impl Into<String>, arguments: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            arguments: arguments.into(),
        }
    }
}

impl std::fmt::Debug for ToolCall {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ToolCall")
            .field("id", &self.id)
            .field("kind", &self.kind)
            .field("function", &self.function)
            .field(
                "encrypted_value",
                &crate::reasoning::redact(&self.encrypted_value),
            )
            .field("metadata", &self.metadata)
            .finish()
    }
}
