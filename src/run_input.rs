//! [`RunAgentInput`]: the request that opens an exchange (`basic/run-input.mdx`).

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::ids::{RunId, ThreadId};
use crate::interrupt::ResumeEntry;
use crate::message::Message;
use crate::tool::Tool;

/// The one message a client sends to open an exchange (RUNIN-01).
///
/// Clients rarely build this directly: [`RunRequest`][crate::client::RunRequest]
/// produces it and the outgoing pipeline strips activity messages, removes
/// unknown material and declares the protocol version before sending
/// (RUNIN-08, RUNIN-12, PROC-32..PROC-34). Servers receive it through
/// [`server::input::admit`][crate::server::input::admit].
///
/// `tools`, `context` and `resume` are plain vectors: absent and empty mean the
/// same thing (RUNIN-20, RUNIN-23) and an empty list is omitted on the wire
/// (RUNIN-21).
///
/// Schema: `$defs/RunAgentInput`. Spec rules: RUNIN-01..RUNIN-31.
#[cfg_attr(
    not(feature = "client"),
    doc = "",
    doc = "[crate::client::RunRequest]: https://docs.rs/agui-sdk/latest/agui_sdk/client/struct.RunRequest.html"
)]
#[cfg_attr(
    not(feature = "server"),
    doc = "",
    doc = "[crate::server::input::admit]: https://docs.rs/agui-sdk/latest/agui_sdk/server/input/fn.admit.html"
)]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct RunAgentInput {
    /// Conversation identifier, minted by the application (BASIC-13).
    pub thread_id: ThreadId,
    /// Identifier of the requested run; never reused on the thread (BASIC-14).
    pub run_id: RunId,
    /// Complete history so far, in order (RUNIN-09, RUNIN-10).
    pub messages: Vec<Message>,
    /// The client's declared version (RUNIN-08, VER-17).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub protocol_version: Option<String>,
    /// Run that spawned this one (RUNIN-07).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_run_id: Option<RunId>,
    /// Starting state; absent means `{}` (RUNIN-24, RUNIN-25).
    #[serde(default, skip_serializing_if = "crate::json::is_absent")]
    pub state: Option<Value>,
    /// Frontend tools the agent may call (RUNIN-19, RUNIN-20).
    /// `None` is absent; `Some(vec![])` is a present `[]`. The two are kept
    /// apart so a re-serialized or echoed input matches what was received
    /// (RUNIN-02, RUNIN-21). Read through [`tools()`](Self::tools).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<Tool>>,
    /// Grounding entries for the model (RUNIN-22, RUNIN-23).
    /// `None` is absent; `Some(vec![])` is a present `[]`. The two are kept
    /// apart so a re-serialized or echoed input matches what was received
    /// (RUNIN-02, RUNIN-21). Read through [`context()`](Self::context).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context: Option<Vec<Context>>,
    /// Opaque pass-through; intermediaries never alter it (RUNIN-26, RUNIN-27).
    #[serde(default, skip_serializing_if = "crate::json::is_absent")]
    pub forwarded_props: Option<Value>,
    /// Answers to the interrupts that ended the previous run (RUNIN-28, INTR-08..INTR-12).
    /// `None` is absent; `Some(vec![])` is a present `[]`. The two are kept
    /// apart so a re-serialized or echoed input matches what was received
    /// (RUNIN-02, RUNIN-21). Read through [`resume()`](Self::resume).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resume: Option<Vec<ResumeEntry>>,
}

impl RunAgentInput {
    /// Frontend tools; empty when absent (RUNIN-20).
    #[must_use]
    pub fn tools(&self) -> &[Tool] {
        self.tools.as_deref().unwrap_or(&[])
    }

    /// Grounding entries; empty when absent (RUNIN-23).
    #[must_use]
    pub fn context(&self) -> &[Context] {
        self.context.as_deref().unwrap_or(&[])
    }

    /// Resume entries; empty when absent.
    #[must_use]
    pub fn resume(&self) -> &[ResumeEntry] {
        self.resume.as_deref().unwrap_or(&[])
    }

    /// Creates the value from its required fields (`thread_id`, `run_id`, `messages`); every optional
    /// field starts absent.
    pub fn new(
        thread_id: impl Into<ThreadId>,
        run_id: impl Into<RunId>,
        messages: Vec<Message>,
    ) -> Self {
        Self {
            thread_id: thread_id.into(),
            run_id: run_id.into(),
            messages,
            protocol_version: None,
            parent_run_id: None,
            state: None,
            tools: None,
            context: None,
            forwarded_props: None,
            resume: None,
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

    /// Sets [`state`](Self::state).
    #[must_use]
    pub fn with_state(self, state: Value) -> Self {
        Self {
            state: crate::json::present(state),
            ..self
        }
    }

    /// Sets [`tools`](Self::tools).
    #[must_use]
    pub fn with_tools(self, tools: impl IntoIterator<Item = Tool>) -> Self {
        Self {
            tools: Some(tools.into_iter().collect()),
            ..self
        }
    }

    /// Sets [`context`](Self::context).
    #[must_use]
    pub fn with_context(self, context: impl IntoIterator<Item = Context>) -> Self {
        Self {
            context: Some(context.into_iter().collect()),
            ..self
        }
    }

    /// Sets [`forwarded_props`](Self::forwarded_props).
    #[must_use]
    pub fn with_forwarded_props(self, forwarded_props: Value) -> Self {
        Self {
            forwarded_props: crate::json::present(forwarded_props),
            ..self
        }
    }

    /// Sets [`resume`](Self::resume).
    #[must_use]
    pub fn with_resume(self, resume: impl IntoIterator<Item = ResumeEntry>) -> Self {
        Self {
            resume: Some(resume.into_iter().collect()),
            ..self
        }
    }
}

/// One grounding entry the agent should make available to the model (RUNIN-22).
///
/// Schema: `$defs/Context`. Spec rules: RUNIN-22.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct Context {
    /// What the value is.
    pub description: String,
    /// The grounding text.
    pub value: String,
}

impl RunAgentInput {
    /// Messages with activity messages removed, as they must be sent (RUNIN-12,
    /// ACT-18). Borrows; allocates nothing.
    #[must_use]
    pub fn sendable_messages(&self) -> SendableMessages<'_> {
        SendableMessages {
            inner: self.messages.iter(),
        }
    }
}

/// Iterator from [`RunAgentInput::sendable_messages`].
#[derive(Debug)]
pub struct SendableMessages<'a> {
    inner: std::slice::Iter<'a, Message>,
}

impl<'a> Iterator for SendableMessages<'a> {
    type Item = &'a Message;

    fn next(&mut self) -> Option<&'a Message> {
        self.inner
            .by_ref()
            .find(|message| !matches!(message, Message::Activity(_)))
    }
}

impl Context {
    /// A grounding entry: `description` says what `value` holds. Both are
    /// required by the schema, so there is nothing else to set.
    pub fn new(description: impl Into<String>, value: impl Into<String>) -> Self {
        let description = description.into();
        let value = value.into();
        Self { description, value }
    }
}
