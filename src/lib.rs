//! AG-UI 1.0 for Rust: the protocol's types, plus a conformant client and
//! server over HTTP with Server-Sent Events and protobuf.
//!
//! AG-UI is an event protocol between an *agent* (the producer) and a *user
//! interface* (the consumer). A client sends one [`RunAgentInput`] to open an
//! exchange; the agent answers with a stream of [`Event`]s bracketed by run
//! lifecycle events. This crate covers both sides:
//!
//! * **Protocol types** (always on): every event, message, content part,
//!   capability and input type in the 1.0 schema, as `#[non_exhaustive]`
//!   structs and enums with constructors and builders. New protocol minors are
//!   therefore additive releases of this crate, never breaking ones.
//! * **The processing pipeline** (always on, sans-IO): the compatibility
//!   boundary, enforcement (strip unknown material, reject malformed known
//!   values), chunk expansion, stream verification and the reducer that builds
//!   message history and state. It has no runtime and no I/O, so it runs the
//!   same under tokio, async-std, a WASM host or a unit test.
//! * **[`client`]** (feature `client`): [`AgentClient`][client::AgentClient]
//!   runs the pipeline over a [`Transport`][client::transport::Transport]. There is no public
//!   path that hands application code an event the pipeline has not processed
//!   (PROC-31).
//! * **[`server`]** (feature `server`): implement [`Agent`][server::Agent],
//!   hand it to a [`Runner`][server::Runner], and serve the result. The runner
//!   owns the run lifecycle and verifies every event before it reaches the
//!   wire, so an agent cannot produce a non-conformant stream.
//!
//! # Where to start
//!
//! | You are writing | Start at |
//! |---|---|
//! | a UI or service that calls an agent | [`client::AgentClient`] |
//! | an agent endpoint | [`server::Agent`], [`server::Runner`], [`server::axum`] (`AguiRequest`/`AguiResponse`) |
//! | your own transport or framework glue | [`pipeline`], [`client::transport::Transport`], [`server::http`] |
//! | tooling over recorded streams | [`event`], [`pipeline::Processor`] |
//!
//! # Feature flags
//!
//! The default build is the protocol layer only: its dependencies are `serde`
//! and `serde_json`, nothing else. Every row below is additive.
//!
//! | Feature | Enables | Extra dependencies | MSRV |
//! |---|---|---|---|
//! | *(none)* | protocol types, [`diagnostics`], [`error`], [`patch`], [`version`], [`cancel`], the sans-IO [`pipeline`] | `serde`, `serde_json` | 1.75 |
//! | `client` | [`client`]: `AgentClient`, `Thread`, `HttpTransport` (SSE, plain `http://`), middleware, observers; needs a tokio runtime | `hyper` (HTTP/1.1), `hyper-util`, `http`, `http-body(-util)`, `tokio` (`time`, `net`), `tower-service`, `bytes`, `memchr`, `pin-project-lite` | 1.75 |
//! | `rustls` | HTTPS for `client` through rustls 0.23 (ring) with the webpki-roots bundle; implies `client` | `hyper-rustls`, `rustls`, `webpki-roots` | 1.75 |
//! | `rustls-native-roots` | also trust the OS root store; implies `rustls` | `rustls-native-certs` (via `hyper-rustls`) | 1.75 |
//! | `native-tls` | HTTPS for `client` through the platform stack (OpenSSL, SChannel, Security.framework); implies `client` | `hyper-tls`, `native-tls`, `tokio-native-tls` | 1.80 |
//! | `server` | [`server`]: `Agent`, `RunContext`, `Runner`, producer verifier, SSE/JSON encoders, [`server::http`]; no runtime | `bytes`, `pin-project-lite` | 1.75 |
//! | `axum` | [`server::axum`]: `FromRequest`/`IntoResponse` types for axum 0.8; implies `server` | `axum-core` 0.5, `http`, `http-body(-util)`, `tokio` (`time`, `net`) | 1.75 |
//! | `proto` | [`proto`] codec and framing; protobuf on whichever of `client`/`server` is on | `bytes` | 1.75 |
//! | `stream` | `futures_core::Stream` for the crate's own async streams | `futures-core` | 1.75 |
//! | `preserve-order` | insertion-ordered JSON objects (`serde_json/preserve_order`), for protobuf byte parity from typed events (PB-12) | `indexmap` (via `serde_json`) | 1.75 |
//! | `tracing` | diagnostics routed to `tracing` (target `agui_sdk::diagnostics`) | `tracing` | 1.75 |
//! | `log` | diagnostics routed to `log` when no tracing subscriber takes them | `log` | 1.75 |
//!
//! With both `rustls` and `native-tls` on (as with `--all-features`), rustls
//! is used unless [`HttpTransportBuilder::tls_backend`][client::HttpTransportBuilder::tls_backend]
//! selects the platform stack. With neither, `build()` refuses an `https://`
//! URL with `config.invalid`: `client` implies no TLS backend, so a client
//! that calls `https://` agents depends on `features = ["rustls"]`.
//!
//! Every row but `native-tls` builds on Rust 1.75, the whole crate's MSRV;
//! `native-tls` needs 1.80, which its advisory-clean `openssl` requires.
//! Dependency
//! requirements are kept loose enough that 1.75-compatible releases satisfy
//! them, so newer toolchains still resolve to the latest versions. Cargo 1.75
//! has no MSRV-aware resolver: on 1.75, generate your lockfile with a cargo
//! ≥ 1.84 and `CARGO_RESOLVER_INCOMPATIBLE_RUST_VERSIONS=fallback cargo
//! update`, then build with `cargo +1.75 --locked` (see the
//! [MSRV policy](https://github.com/MattJackson/agui-sdk/blob/main/docs/API.md#10-msrv)).
//!
//! With neither `tracing` nor `log`, diagnostics go to standard error: a
//! warning the protocol requires is never silent by default (VER-11).
//! Silencing is explicit, through [`diagnostics::NullSink`].
//!
//! # Conventions
//!
//! * Module paths name the spec area: [`lifecycle`], [`text`], [`tool`],
//!   [`state`], [`activity`], [`reasoning`], [`subagent`], [`passthrough`]
//!   hold that area's events and helpers, and [`event::Event`] gathers all 31.
//!   Event structs drop the schema's `Event` suffix
//!   (`$defs/TextMessageStartEvent` is [`text::TextMessageStart`]); the
//!   rustdoc of each type names its `$defs` entry.
//! * Rule identifiers such as `PROC-31` refer to the requirement ledger mapped
//!   in [`docs/SPEC-MAP.md`](https://github.com/MattJackson/agui-sdk/blob/main/docs/SPEC-MAP.md).
//! * Open JSON (state, payloads, `rawEvent`) is [`serde_json::Value`]; open
//!   objects are [`json::JsonObject`]. **Public dependencies are `serde` 1
//!   (its traits: protocol types implement `Serialize`/`Deserialize`, and
//!   typed-state APIs such as `server::Agent::State` take
//!   `Serialize`/`DeserializeOwned` bounds) and `serde_json` 1 (`Value`)**;
//!   no other third-party type is in a signature: bytes are
//!   [`wire::Chunk`], async streams are the crate's own types with inherent
//!   `next()`/`poll_next()`, and cancellation is [`cancel::CancelToken`].
//!   Two documented exceptions are opt-in features whose whole purpose is a
//!   third-party trait: `stream` (impls of `futures_core::Stream`, plus
//!   `FrameStream::from_stream`) and `axum` (impls of axum-core's
//!   `FromRequest`/`IntoResponse`). A new major of futures-core or axum-core
//!   is supported through a new feature, never by changing an existing one.
//! * The derived `Deserialize` impls on protocol types are *unchecked
//!   decoding of trusted, stored data* (your own database, a test fixture).
//!   They are not the receive path: they do not strip unknown members with a
//!   warning, fail on an unknown closed-set value (where the pipeline strips,
//!   PROC-17), and never verify the stream. Received frames go through
//!   [`pipeline::Processor`] (or the client), which does all of that.
//! * Errors are one type, [`Error`], carrying a stable [`ErrorCode`].
//!   Non-fatal findings are [`diagnostics::Diagnostic`]s, delivered to a
//!   [`diagnostics::DiagnosticSink`], never mixed into the event stream.
#![cfg_attr(
    not(feature = "client"),
    doc = "",
    doc = "[client::AgentClient]: https://docs.rs/agui-sdk/latest/agui_sdk/client/struct.AgentClient.html"
)]
#![cfg_attr(
    not(feature = "client"),
    doc = "",
    doc = "[client::transport::Transport]: https://docs.rs/agui-sdk/latest/agui_sdk/client/transport/trait.Transport.html"
)]
#![cfg_attr(
    not(feature = "server"),
    doc = "",
    doc = "[server::Agent]: https://docs.rs/agui-sdk/latest/agui_sdk/server/trait.Agent.html"
)]
#![cfg_attr(
    not(feature = "server"),
    doc = "",
    doc = "[server::Runner]: https://docs.rs/agui-sdk/latest/agui_sdk/server/struct.Runner.html"
)]
#![cfg_attr(
    not(feature = "client"),
    doc = "",
    doc = "[`client::AgentClient`]: https://docs.rs/agui-sdk/latest/agui_sdk/client/struct.AgentClient.html"
)]
#![cfg_attr(
    not(feature = "server"),
    doc = "",
    doc = "[`server::Agent`]: https://docs.rs/agui-sdk/latest/agui_sdk/server/trait.Agent.html"
)]
#![cfg_attr(
    not(feature = "server"),
    doc = "",
    doc = "[`server::Runner`]: https://docs.rs/agui-sdk/latest/agui_sdk/server/struct.Runner.html"
)]
#![cfg_attr(
    not(feature = "client"),
    doc = "",
    doc = "[`client::transport::Transport`]: https://docs.rs/agui-sdk/latest/agui_sdk/client/transport/trait.Transport.html"
)]
#![cfg_attr(
    not(feature = "client"),
    doc = "",
    doc = "[`client`]: https://docs.rs/agui-sdk/latest/agui_sdk/client/index.html"
)]
#![cfg_attr(
    not(feature = "server"),
    doc = "",
    doc = "[`server`]: https://docs.rs/agui-sdk/latest/agui_sdk/server/index.html"
)]
#![cfg_attr(
    not(feature = "server"),
    doc = "",
    doc = "[`server::http`]: https://docs.rs/agui-sdk/latest/agui_sdk/server/http/index.html"
)]
#![cfg_attr(
    not(feature = "axum"),
    doc = "",
    doc = "[`server::axum`]: https://docs.rs/agui-sdk/latest/agui_sdk/server/axum/index.html"
)]
#![cfg_attr(
    not(feature = "proto"),
    doc = "",
    doc = "[`proto`]: https://docs.rs/agui-sdk/latest/agui_sdk/proto/index.html"
)]
#![cfg_attr(
    not(feature = "client"),
    doc = "",
    doc = "[client::HttpTransportBuilder::tls_backend]: https://docs.rs/agui-sdk/latest/agui_sdk/client/transport/struct.HttpTransportBuilder.html#method.tls_backend"
)]
#![cfg_attr(
    not(any(feature = "client", feature = "server", feature = "proto")),
    doc = "",
    doc = "[`wire::Chunk`]: https://docs.rs/agui-sdk/latest/agui_sdk/wire/struct.Chunk.html"
)]
#![forbid(unsafe_code)]
#![deny(missing_docs)]
// Keep `#[must_use]` on every pure accessor, builder and setter returning
// `Self`, so a dropped builder or a dropped lazy run is a warning (CI denies
// it). These two lints catch most new candidates, but not macro-generated
// items or every accessor: mark those by hand (`MustUseDoctests` pins a few).
#![warn(clippy::must_use_candidate, clippy::return_self_not_must_use)]
#![cfg_attr(docsrs, feature(doc_cfg))]

#[macro_use]
mod sparse;

/// The pinned spec cache and protobuf corpus helpers, shared with the
/// integration tests (std only): one copy for every unit test that reads
/// the upstream data.
#[cfg(test)]
#[path = "../tests/proto_support/mod.rs"]
mod test_spec;

pub mod ids;
pub mod json;
pub mod metadata;

pub mod event;

pub mod activity;
pub mod lifecycle;
pub mod passthrough;
pub mod reasoning;
pub mod state;
pub mod subagent;
pub mod text;
pub mod tool;

pub mod capability;
pub mod content;
pub mod interrupt;
pub mod message;
pub mod run_input;
pub mod usage;

pub mod cancel;
pub mod diagnostics;
pub mod error;
pub mod patch;
pub mod version;
pub mod wire;

pub mod pipeline;
pub mod prelude;

#[cfg(feature = "proto")]
#[cfg_attr(docsrs, doc(cfg(feature = "proto")))]
pub mod proto;

#[cfg(feature = "client")]
#[cfg_attr(docsrs, doc(cfg(feature = "client")))]
pub mod client;

#[cfg(feature = "server")]
#[cfg_attr(docsrs, doc(cfg(feature = "server")))]
pub mod server;

pub use error::{BoxError, Error, ErrorCode, ErrorKind, Result};
pub use event::{Event, EventType};
pub use message::Message;
pub use run_input::RunAgentInput;

/// The protocol version this crate implements and declares, in `MAJOR.MINOR`
/// form: on outgoing [`RunAgentInput::protocol_version`] (VER-17) and on
/// [`lifecycle::RunStarted::protocol_version`] from the server (VER-18).
pub const PROTOCOL_VERSION: &str = "1.0";

/// The README's quick start, compiled by `cargo test --doc` so it cannot drift
/// from the API. Its examples need both the `client` and `axum` features.
#[cfg(all(doctest, feature = "client", feature = "axum"))]
#[doc = include_str!("../README.md")]
pub struct ReadmeDoctests;

/// `#[must_use]` on pure accessors and on the emitter's open-item handles,
/// compiled by `cargo test --doc`. Each `compile_fail` case has a passing
/// control that differs only by `let _ =`, so it cannot fail for another
/// reason. The `deny` sits on the function: rustdoc's injected
/// `#![allow(unused)]` would override a crate-level one.
///
/// ```compile_fail
/// #[deny(unused_must_use)]
/// fn f(id: &agui_sdk::ids::RunId) {
///     id.as_str();
/// }
/// ```
/// ```
/// #[deny(unused_must_use)]
/// fn f(id: &agui_sdk::ids::RunId) {
///     let _ = id.as_str();
/// }
/// ```
/// ```compile_fail
/// #[deny(unused_must_use)]
/// fn f(code: agui_sdk::ErrorCode) {
///     code.kind();
/// }
/// ```
/// ```
/// #[deny(unused_must_use)]
/// fn f(code: agui_sdk::ErrorCode) {
///     let _ = code.kind();
/// }
/// ```
/// ```compile_fail
/// #[deny(unused_must_use)]
/// fn f(code: agui_sdk::diagnostics::DiagnosticCode) {
///     code.as_str();
/// }
/// ```
/// ```
/// #[deny(unused_must_use)]
/// fn f(code: agui_sdk::diagnostics::DiagnosticCode) {
///     let _ = code.as_str();
/// }
/// ```
/// ```compile_fail
/// #[deny(unused_must_use)]
/// fn f(chunk: &agui_sdk::wire::Chunk) {
///     chunk.len();
/// }
/// ```
/// ```
/// #[deny(unused_must_use)]
/// fn f(chunk: &agui_sdk::wire::Chunk) {
///     let _ = chunk.len();
/// }
/// ```
/// ```compile_fail
/// #[deny(unused_must_use)]
/// fn f(ctx: &agui_sdk::server::RunContext) {
///     ctx.is_cancelled();
/// }
/// ```
/// ```
/// #[deny(unused_must_use)]
/// fn f(ctx: &agui_sdk::server::RunContext) {
///     let _ = ctx.is_cancelled();
/// }
/// ```
/// ```compile_fail
/// #[deny(unused_must_use)]
/// async fn f(e: &agui_sdk::server::emit::Emitter) -> agui_sdk::Result<()> {
///     e.start_step("s").await?;
///     Ok(())
/// }
/// ```
/// ```
/// #[deny(unused_must_use)]
/// async fn f(e: &agui_sdk::server::emit::Emitter) -> agui_sdk::Result<()> {
///     let _ = e.start_step("s").await?;
///     Ok(())
/// }
/// ```
#[cfg(all(doctest, feature = "server"))]
pub struct MustUseDoctests;
