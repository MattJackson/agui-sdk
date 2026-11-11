//! The common imports: `use agui_sdk::prelude::*;`.
//!
//! Protocol types are always included; client and server items follow their
//! features. Adding an item here is a minor change; nothing is removed
//! outside a major release.

pub use crate::cancel::CancelToken;
pub use crate::error::{Error, ErrorCode, Result};
pub use crate::event::{Event, EventType};
pub use crate::ids::{InterruptId, MessageId, RunId, SubagentRunId, ThreadId, ToolCallId};
pub use crate::message::Message;
pub use crate::run_input::RunAgentInput;

#[cfg(feature = "client")]
#[cfg_attr(docsrs, doc(cfg(feature = "client")))]
pub use crate::client::{AgentClient, RunHandle, RunReport, RunRequest, RunUpdate, Thread};

#[cfg(feature = "server")]
#[cfg_attr(docsrs, doc(cfg(feature = "server")))]
pub use crate::server::{Agent, RunContext, RunOutcome, Runner};
