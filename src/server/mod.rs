//! The server: serve an agent as a conformant AG-UI producer.
//!
//! ```no_run
//! use agui_sdk::server::{Agent, RunContext, RunOutcome, Runner};
//!
//! struct Echo;
//!
//! impl Agent for Echo {
//!     type State = serde_json::Value;
//!
//!     async fn run(&self, ctx: RunContext<Self::State>) -> Result<RunOutcome, agui_sdk::BoxError> {
//!         let last = ctx.messages().last().and_then(|m| m.text()).unwrap_or("").to_owned();
//!         ctx.emitter().say(last).await?;
//!         Ok(RunOutcome::success())
//!     }
//! }
//!
//! let runner = Runner::new(Echo);
//! # let _ = runner;
//! ```
//!
//! Division of labour:
//!
//! * The agent ([`Agent`]) emits content through its [`RunContext`] and
//!   returns a [`RunOutcome`]. It never emits lifecycle events.
//! * The [`Runner`] emits `RUN_STARTED` (with its own `protocolVersion`,
//!   VER-18) and exactly one terminal; closes anything left open before any
//!   `RUN_FINISHED` (LIFE-12); derives `pendingToolCallIds` (TOOL-31); turns
//!   agent errors, panics, verifier refusals and queue overflow into
//!   `RUN_ERROR` (LIFE-13); and runs every event through the
//!   [`ProducerVerifier`](crate::pipeline::verify::ProducerVerifier), so an invalid event never
//!   reaches the wire (ARCH-04).
//! * [`input::admit`] judges the request before any stream exists: malformed
//!   input is a 4xx through the transport's error path, never `RUN_ERROR`
//!   (RUNIN-29, SSE-12); unknown members are stripped with a warning
//!   (RUNIN-30); newer minors are served with a warning (VER-22, VER-23).
//! * [`encode`] writes the stream as SSE or protobuf, chosen by `Accept`
//!   (PB-03..PB-05). [`http`] does request handling without a framework;
//!   [`axum`] (feature `axum`) provides `FromRequest`/`IntoResponse` types
//!   to mount in your own axum 0.8 `Router`.
//!
//! The server needs no async runtime: the runner drives the agent's future
//! inside the stream it returns, and the only timer (SSE keep-alives) lives
//! in the framework glue.
#![cfg_attr(
    not(feature = "axum"),
    doc = "",
    doc = "[`axum`]: https://docs.rs/agui-sdk/latest/agui_sdk/server/axum/index.html"
)]

mod agent;
mod context;
pub mod emit;
pub mod encode;
mod extensions;
pub mod helpers;
pub mod http;
pub mod input;
mod runner;
pub mod state;

#[cfg(feature = "axum")]
#[cfg_attr(docsrs, doc(cfg(feature = "axum")))]
pub mod axum;

pub use crate::cancel::{CancelToken, Cancelled};
pub use crate::error::BoxError;
pub use agent::{Agent, BoxAgent, DynAgent, RunOutcome};
pub use context::RunContext;
pub use extensions::Extensions;
pub use runner::{ReplayedRun, RunStream, Runner, RunnerConfig, Transformer};
