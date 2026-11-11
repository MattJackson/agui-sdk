//! Conversation history: [`Message`] and its seven role structs.
//!
//! One representation serves every purpose: run input, `MESSAGES_SNAPSHOT`,
//! the reducer's history and reports. Messages are tagged by `role` on the
//! wire. Activity and reasoning messages are client-side records the reducer
//! builds from events; activity messages are stripped from outgoing input
//! (RUNIN-12) and both survive a snapshot that does not mention their role
//! (STATE-07, STATE-08).

use std::fmt;

use serde::{Deserialize, Deserializer, Serialize};

use crate::content::Content;
use crate::ids::{MessageId, SubagentRunId, ToolCallId};
use crate::json::{deserialize_tagged, JsonObject, Tagged};
use crate::metadata::Metadata;
use crate::reasoning::{redact, Redacted};
use crate::tool::ToolCall;

/// Expands `$body` once per role with `$m` bound to the role struct.
macro_rules! each_message {
    ($message:expr, $m:ident => $body:expr) => {
        match $message {
            Message::Developer($m) => $body,
            Message::System($m) => $body,
            Message::Assistant($m) => $body,
            Message::User($m) => $body,
            Message::Tool($m) => $body,
            Message::Activity($m) => $body,
            Message::Reasoning($m) => $body,
        }
    };
}

/// Every message role (`$defs/Role`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
#[non_exhaustive]
pub enum Role {
    /// [`DeveloperMessage`].
    Developer,
    /// [`SystemMessage`].
    System,
    /// [`AssistantMessage`].
    Assistant,
    /// [`UserMessage`].
    User,
    /// [`ToolMessage`].
    Tool,
    /// [`ActivityMessage`].
    Activity,
    /// [`ReasoningMessage`].
    Reasoning,
}

/// A message of any role (`$defs/Message`).
///
/// ```
/// use agui_sdk::message::Message;
/// let m = Message::user("What's the weather?");
/// assert_eq!(m.text(), Some("What's the weather?"));
/// ```
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "role", rename_all = "lowercase")]
#[non_exhaustive]
pub enum Message {
    /// Developer instructions.
    Developer(DeveloperMessage),
    /// System prompt.
    System(SystemMessage),
    /// Agent message, possibly with tool calls.
    Assistant(AssistantMessage),
    /// End-user message.
    User(UserMessage),
    /// Tool result.
    Tool(ToolMessage),
    /// Client-side activity record.
    Activity(ActivityMessage),
    /// Visible reasoning.
    Reasoning(ReasoningMessage),
}

impl Message {
    /// A user text message with a random id.
    pub fn user(text: impl Into<String>) -> Self {
        Message::User(UserMessage::new(
            MessageId::random(),
            Content::Text(text.into()),
        ))
    }

    /// An assistant text message with a random id.
    pub fn assistant(text: impl Into<String>) -> Self {
        Message::Assistant(AssistantMessage::new(MessageId::random()).with_content(text))
    }

    /// A system message with a random id.
    pub fn system(text: impl Into<String>) -> Self {
        Message::System(SystemMessage::new(MessageId::random(), text))
    }

    /// A developer message with a random id.
    pub fn developer(text: impl Into<String>) -> Self {
        Message::Developer(DeveloperMessage::new(MessageId::random(), text))
    }

    /// A tool message answering `tool_call_id` with a random id (TOOL-33).
    pub fn tool(tool_call_id: impl Into<ToolCallId>, content: impl Into<Content>) -> Self {
        Message::Tool(ToolMessage::new(
            MessageId::random(),
            content.into(),
            tool_call_id,
        ))
    }

    /// The message id.
    #[must_use]
    pub fn id(&self) -> &MessageId {
        each_message!(self, m => &m.id)
    }

    /// The role.
    #[must_use]
    pub fn role(&self) -> Role {
        // Variant and role names coincide.
        macro_rules! role_of {
            ($($name:ident)*) => {
                match self { $(Message::$name(_) => Role::$name,)* }
            };
        }
        role_of!(Developer System Assistant User Tool Activity Reasoning)
    }

    /// The text content when it is plain text (a string, or for
    /// assistant/system/developer/reasoning, the accumulated text). `None` for
    /// part lists and activity messages.
    #[must_use]
    pub fn text(&self) -> Option<&str> {
        match self {
            Message::Developer(m) => Some(&m.content),
            Message::System(m) => Some(&m.content),
            Message::Assistant(m) => m.content.as_deref(),
            Message::User(m) => m.content.as_text(),
            Message::Tool(m) => m.content.as_text(),
            Message::Activity(_) => None,
            Message::Reasoning(m) => Some(&m.content),
        }
    }

    /// The owning subagent invocation, if attributed.
    #[must_use]
    pub fn subagent_run_id(&self) -> Option<&SubagentRunId> {
        each_message!(self, m => m.subagent_run_id.as_ref())
    }

    /// The message's metadata, if any.
    #[must_use]
    pub fn metadata(&self) -> Option<&Metadata> {
        each_message!(self, m => m.metadata.as_ref())
    }

    /// Tool calls carried by an assistant message; empty otherwise.
    #[must_use]
    pub fn tool_calls(&self) -> &[ToolCall] {
        match self {
            Message::Assistant(m) => m.tool_calls.as_deref().unwrap_or(&[]),
            _ => &[],
        }
    }
}

/// Instructions from the application developer.
///
/// Schema: `$defs/DeveloperMessage`. Spec rules: TEXT-12.
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct DeveloperMessage {
    /// Message identifier, unique within the thread.
    pub id: MessageId,
    /// Message text.
    pub content: String,
    /// Optional author display name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Opaque provider artefact; stored and returned unmodified (REAS-14, REAS-15).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub encrypted_value: Option<String>,
    /// Annotations merged from the message's events (META-06, META-08).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<Metadata>,
    /// Owning subagent invocation; absent means the parent agent (SUB-01).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subagent_run_id: Option<SubagentRunId>,
}

impl DeveloperMessage {
    /// Creates the value from its required fields (`id`, `content`); every optional
    /// field starts absent.
    pub fn new(id: impl Into<MessageId>, content: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            content: content.into(),
            name: None,
            encrypted_value: None,
            metadata: None,
            subagent_run_id: None,
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

    /// Sets [`subagent_run_id`](Self::subagent_run_id).
    #[must_use]
    pub fn with_subagent_run_id(self, subagent_run_id: impl Into<SubagentRunId>) -> Self {
        Self {
            subagent_run_id: Some(subagent_run_id.into()),
            ..self
        }
    }
}

/// A system prompt.
///
/// Schema: `$defs/SystemMessage`. Spec rules: TEXT-12.
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct SystemMessage {
    /// Message identifier, unique within the thread.
    pub id: MessageId,
    /// Message text.
    pub content: String,
    /// Optional author display name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Opaque provider artefact; stored and returned unmodified (REAS-14, REAS-15).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub encrypted_value: Option<String>,
    /// Annotations merged from the message's events (META-06, META-08).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<Metadata>,
    /// Owning subagent invocation; absent means the parent agent (SUB-01).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subagent_run_id: Option<SubagentRunId>,
}

impl SystemMessage {
    /// Creates the value from its required fields (`id`, `content`); every optional
    /// field starts absent.
    pub fn new(id: impl Into<MessageId>, content: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            content: content.into(),
            name: None,
            encrypted_value: None,
            metadata: None,
            subagent_run_id: None,
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

    /// Sets [`subagent_run_id`](Self::subagent_run_id).
    #[must_use]
    pub fn with_subagent_run_id(self, subagent_run_id: impl Into<SubagentRunId>) -> Self {
        Self {
            subagent_run_id: Some(subagent_run_id.into()),
            ..self
        }
    }
}

/// A message from the agent, optionally carrying tool calls.
///
/// Schema: `$defs/AssistantMessage`. Spec rules: TEXT-12, TOOL-01, SUB-26.
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct AssistantMessage {
    /// Message identifier, unique within the thread.
    pub id: MessageId,
    /// Accumulated text; absent for a tool-call-only message.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    /// Calls attached through `parentMessageId` (TOOL-01).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ToolCall>>,
    /// Optional author display name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Opaque provider artefact; stored and returned unmodified (REAS-14, REAS-15).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub encrypted_value: Option<String>,
    /// Annotations merged from the message's events (META-06, META-08).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<Metadata>,
    /// Owning subagent invocation; absent means the parent agent (SUB-01).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subagent_run_id: Option<SubagentRunId>,
}

impl AssistantMessage {
    /// Creates the value from its required fields (`id`); every optional
    /// field starts absent.
    pub fn new(id: impl Into<MessageId>) -> Self {
        Self {
            id: id.into(),
            content: None,
            tool_calls: None,
            name: None,
            encrypted_value: None,
            metadata: None,
            subagent_run_id: None,
        }
    }

    /// Sets [`content`](Self::content).
    #[must_use]
    pub fn with_content(self, content: impl Into<String>) -> Self {
        Self {
            content: Some(content.into()),
            ..self
        }
    }

    /// Sets [`tool_calls`](Self::tool_calls).
    #[must_use]
    pub fn with_tool_calls(self, tool_calls: impl IntoIterator<Item = ToolCall>) -> Self {
        Self {
            tool_calls: Some(tool_calls.into_iter().collect()),
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

    /// Sets [`subagent_run_id`](Self::subagent_run_id).
    #[must_use]
    pub fn with_subagent_run_id(self, subagent_run_id: impl Into<SubagentRunId>) -> Self {
        Self {
            subagent_run_id: Some(subagent_run_id.into()),
            ..self
        }
    }
}

/// A message from the end user, text or multimodal parts (CHG-04).
///
/// Schema: `$defs/UserMessage`. Spec rules: TEXT-12, CHG-04.
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct UserMessage {
    /// Message identifier, unique within the thread.
    pub id: MessageId,
    /// Text or an ordered list of content parts.
    pub content: Content,
    /// Optional author display name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Opaque provider artefact; stored and returned unmodified (REAS-14, REAS-15).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub encrypted_value: Option<String>,
    /// Annotations merged from the message's events (META-06, META-08).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<Metadata>,
    /// Owning subagent invocation; absent means the parent agent (SUB-01).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subagent_run_id: Option<SubagentRunId>,
}

impl UserMessage {
    /// Creates the value from its required fields (`id`, `content`); every optional
    /// field starts absent.
    pub fn new(id: impl Into<MessageId>, content: Content) -> Self {
        Self {
            id: id.into(),
            content,
            name: None,
            encrypted_value: None,
            metadata: None,
            subagent_run_id: None,
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

    /// Sets [`subagent_run_id`](Self::subagent_run_id).
    #[must_use]
    pub fn with_subagent_run_id(self, subagent_run_id: impl Into<SubagentRunId>) -> Self {
        Self {
            subagent_run_id: Some(subagent_run_id.into()),
            ..self
        }
    }
}

/// The answer to a tool call, minted by `TOOL_CALL_RESULT` or sent by the
/// client for a frontend tool (TOOL-33).
///
/// Schema: `$defs/ToolMessage`. Spec rules: TOOL-11, TOOL-14, TOOL-33, META-11.
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct ToolMessage {
    /// Message identifier, unique within the thread.
    pub id: MessageId,
    /// Result content, same shape as the result event (TOOL-14).
    pub content: Content,
    /// The call this message answers.
    pub tool_call_id: ToolCallId,
    /// Failure description when the tool failed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Opaque provider artefact (REAS-15).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub encrypted_value: Option<String>,
    /// Annotations merged from `TOOL_CALL_RESULT` (META-11).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<Metadata>,
    /// Owning subagent invocation (SUB-01).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subagent_run_id: Option<SubagentRunId>,
}

impl ToolMessage {
    /// Creates the value from its required fields (`id`, `content`, `tool_call_id`); every optional
    /// field starts absent.
    pub fn new(
        id: impl Into<MessageId>,
        content: Content,
        tool_call_id: impl Into<ToolCallId>,
    ) -> Self {
        Self {
            id: id.into(),
            content,
            tool_call_id: tool_call_id.into(),
            error: None,
            encrypted_value: None,
            metadata: None,
            subagent_run_id: None,
        }
    }

    /// Sets [`error`](Self::error).
    #[must_use]
    pub fn with_error(self, error: impl Into<String>) -> Self {
        Self {
            error: Some(error.into()),
            ..self
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

    /// Sets [`subagent_run_id`](Self::subagent_run_id).
    #[must_use]
    pub fn with_subagent_run_id(self, subagent_run_id: impl Into<SubagentRunId>) -> Self {
        Self {
            subagent_run_id: Some(subagent_run_id.into()),
            ..self
        }
    }
}

/// A client-only structured activity record built from `ACTIVITY_*` events.
/// Stripped from outgoing run input (RUNIN-12, ACT-18).
///
/// Schema: `$defs/ActivityMessage`. Spec rules: ACT-02..ACT-07, ACT-17, ACT-18.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct ActivityMessage {
    /// Message identifier, unique within the thread.
    pub id: MessageId,
    /// Open activity kind (ACT-01).
    pub activity_type: String,
    /// Activity payload; always an object (ACT-15).
    pub content: JsonObject,
    /// Accumulated metadata; survives a replacing snapshot (ACT-05, META-12).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<Metadata>,
    /// Owner; re-minted by a replacing snapshot (ACT-06, ACT-07).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subagent_run_id: Option<SubagentRunId>,
}

impl ActivityMessage {
    /// Creates the value from its required fields (`id`, `activity_type`, `content`); every optional
    /// field starts absent.
    pub fn new(
        id: impl Into<MessageId>,
        activity_type: impl Into<String>,
        content: JsonObject,
    ) -> Self {
        Self {
            id: id.into(),
            activity_type: activity_type.into(),
            content,
            metadata: None,
            subagent_run_id: None,
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

/// Visible reasoning text assembled from `REASONING_MESSAGE_*` events.
///
/// Schema: `$defs/ReasoningMessage`. Spec rules: REAS-14, REAS-15, META-09, STATE-07.
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct ReasoningMessage {
    /// Message identifier, unique within the thread.
    pub id: MessageId,
    /// Accumulated reasoning text.
    pub content: String,
    /// Opaque provider artefact (REAS-14, REAS-15).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub encrypted_value: Option<String>,
    /// Annotations merged from the message's events (META-09).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<Metadata>,
    /// Owning subagent invocation (SUB-21).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subagent_run_id: Option<SubagentRunId>,
}

impl ReasoningMessage {
    /// Creates the value from its required fields (`id`, `content`); every optional
    /// field starts absent.
    pub fn new(id: impl Into<MessageId>, content: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            content: content.into(),
            encrypted_value: None,
            metadata: None,
            subagent_run_id: None,
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

    /// Sets [`subagent_run_id`](Self::subagent_run_id).
    #[must_use]
    pub fn with_subagent_run_id(self, subagent_run_id: impl Into<SubagentRunId>) -> Self {
        Self {
            subagent_run_id: Some(subagent_run_id.into()),
            ..self
        }
    }
}
impl From<DeveloperMessage> for Message {
    fn from(m: DeveloperMessage) -> Self {
        Message::Developer(m)
    }
}
impl From<SystemMessage> for Message {
    fn from(m: SystemMessage) -> Self {
        Message::System(m)
    }
}
impl From<AssistantMessage> for Message {
    fn from(m: AssistantMessage) -> Self {
        Message::Assistant(m)
    }
}
impl From<UserMessage> for Message {
    fn from(m: UserMessage) -> Self {
        Message::User(m)
    }
}
impl From<ToolMessage> for Message {
    fn from(m: ToolMessage) -> Self {
        Message::Tool(m)
    }
}
impl From<ActivityMessage> for Message {
    fn from(m: ActivityMessage) -> Self {
        Message::Activity(m)
    }
}
impl From<ReasoningMessage> for Message {
    fn from(m: ReasoningMessage) -> Self {
        Message::Reasoning(m)
    }
}

impl Tagged for Message {
    const TAG: &'static str = "role";
    const EXPECTING: &'static str = "a message object";
    type Tag = Role;

    fn from_tag<'de, D: Deserializer<'de>>(role: Role, rest: D) -> Result<Self, D::Error> {
        Ok(match role {
            Role::Developer => Message::Developer(DeveloperMessage::deserialize(rest)?),
            Role::System => Message::System(SystemMessage::deserialize(rest)?),
            Role::Assistant => Message::Assistant(AssistantMessage::deserialize(rest)?),
            Role::User => Message::User(UserMessage::deserialize(rest)?),
            Role::Tool => Message::Tool(ToolMessage::deserialize(rest)?),
            Role::Activity => Message::Activity(ActivityMessage::deserialize(rest)?),
            Role::Reasoning => Message::Reasoning(ReasoningMessage::deserialize(rest)?),
        })
    }
}

impl<'de> Deserialize<'de> for Message {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserialize_tagged(deserializer)
    }
}

// Debug output never shows encrypted values or reasoning text: both are
// provider artefacts that can carry model internals, and debug output ends
// up in logs.

impl fmt::Debug for DeveloperMessage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DeveloperMessage")
            .field("id", &self.id)
            .field("content", &self.content)
            .field("name", &self.name)
            .field("encrypted_value", &redact(&self.encrypted_value))
            .field("metadata", &self.metadata)
            .field("subagent_run_id", &self.subagent_run_id)
            .finish()
    }
}

impl fmt::Debug for SystemMessage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SystemMessage")
            .field("id", &self.id)
            .field("content", &self.content)
            .field("name", &self.name)
            .field("encrypted_value", &redact(&self.encrypted_value))
            .field("metadata", &self.metadata)
            .field("subagent_run_id", &self.subagent_run_id)
            .finish()
    }
}

impl fmt::Debug for AssistantMessage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AssistantMessage")
            .field("id", &self.id)
            .field("content", &self.content)
            .field("tool_calls", &self.tool_calls)
            .field("name", &self.name)
            .field("encrypted_value", &redact(&self.encrypted_value))
            .field("metadata", &self.metadata)
            .field("subagent_run_id", &self.subagent_run_id)
            .finish()
    }
}

impl fmt::Debug for UserMessage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("UserMessage")
            .field("id", &self.id)
            .field("content", &self.content)
            .field("name", &self.name)
            .field("encrypted_value", &redact(&self.encrypted_value))
            .field("metadata", &self.metadata)
            .field("subagent_run_id", &self.subagent_run_id)
            .finish()
    }
}

impl fmt::Debug for ToolMessage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ToolMessage")
            .field("id", &self.id)
            .field("content", &self.content)
            .field("tool_call_id", &self.tool_call_id)
            .field("error", &self.error)
            .field("encrypted_value", &redact(&self.encrypted_value))
            .field("metadata", &self.metadata)
            .field("subagent_run_id", &self.subagent_run_id)
            .finish()
    }
}

impl fmt::Debug for ReasoningMessage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ReasoningMessage")
            .field("id", &self.id)
            .field("content", &Redacted)
            .field("encrypted_value", &redact(&self.encrypted_value))
            .field("metadata", &self.metadata)
            .field("subagent_run_id", &self.subagent_run_id)
            .finish()
    }
}
