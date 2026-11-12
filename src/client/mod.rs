//! The client: run agents over HTTP (or any [`Transport`]) through the full
//! processing pipeline.
//!
//! ```no_run
//! # async fn demo() -> agui_sdk::Result<()> {
//! use agui_sdk::client::{AgentClient, RunRequest};
//!
//! // An https:// URL needs the `rustls` (or `native-tls`) feature; `client`
//! // alone speaks plain http://.
//! let client = AgentClient::http("https://agent.example/run").bearer_token("t").build()?;
//!
//! // Await the whole run...
//! let report = client.run(RunRequest::new().user("Hello")).await?;
//! println!("{:?}", report.status);
//!
//! // ...or iterate it as it streams (inherent `next()`, no extra crate).
//! let mut run = client.run(RunRequest::new().user("Hello again"));
//! while let Some(update) = run.next().await {
//!     println!("{:?}", update?.event.event_type());
//! }
//! # Ok(())
//! # }
//! ```
//!
//! Shape of the API:
//!
//! * [`AgentClient`] holds configuration and is cheap to clone. [`AgentClient::run`]
//!   takes a [`RunRequest`] and returns a lazy [`RunHandle`]: an async
//!   iterator of [`RunUpdate`]s (inherent `next()`; `futures_core::Stream`
//!   with feature `stream`) that is also `IntoFuture<Output = Result<RunReport>>`.
//!   It is `'static` and `Send`, so it can be driven on a spawned task.
//! * [`Observer`]s get synchronous read-only callbacks, [`subscriber::Subscriber`]s can
//!   stop or mutate delivery, and [`Middleware`] rewrites raw frames before
//!   enforcement.
//! * [`Thread`] carries history, state, interrupts and pending frontend tool
//!   calls across runs, and enforces the conversation rules before sending
//!   (resume coverage and expiry, unanswered calls, approval).
//! * [`transport`] has the [`Transport`] trait and the HTTP implementation
//!   (hyper 1, HTTP/1.1; needs a tokio runtime, because hyper-util spawns
//!   connection tasks).
//!
//! Every event an application sees went through compatibility translation,
//! enforcement, chunk expansion and verification (PROC-31). A `RUN_ERROR` from
//! the agent is a delivered outcome ([`RunStatus::Errored`]), while a stream
//! the pipeline rejects is an [`Error`]; the two are never
//! presented as one another (LIFE-54). A truncated stream is an error with
//! code `stream.truncated` and, once the run has started, a partial report;
//! no `RUN_FINISHED` is ever synthesized (TRN-09, LIFE-20).

pub mod capabilities;
pub mod middleware;
mod observer;
mod request;
mod run;
pub mod subscriber;
mod thread;
pub mod transport;

pub use crate::cancel::{CancelToken, Cancelled};
pub use crate::pipeline::reduce::PendingToolCall;
pub use middleware::{
    map_frames, Frame, FrameSource, FrameStream, Middleware, MiddlewareRequest, Next,
};
pub use observer::{Observer, RunInfo, RunView};
pub use request::RunRequest;
pub use run::{RunFuture, RunHandle, RunReport, RunStatus, RunUpdate};
pub use thread::{ApprovedToolCall, Clock, DecidedBy, SchemaCheck, Thread, ThreadSnapshot};
pub use transport::{HttpTransport, HttpTransportBuilder, TlsBackend, TlsRoots, Transport};

use std::sync::Arc;

use crate::diagnostics::{Diagnostic, DiagnosticSink, Scope};
use crate::error::{Error, ErrorCode, Result};
use crate::ids::{RunId, ThreadId};
use crate::pipeline::Limits;
use crate::run_input::RunAgentInput;
use crate::version::{PeerPin, PeerVersion};

/// Runs agents. Cheap to clone: configuration is shared.
///
/// `T` is the transport; [`HttpTransport`] by default.
pub struct AgentClient<T = HttpTransport> {
    transport: Arc<T>,
    config: Arc<ClientConfig>,
}

struct ClientConfig {
    middleware: Vec<Arc<dyn Middleware>>,
    observers: Vec<Arc<dyn Observer>>,
    subscribers: Vec<Arc<subscriber::Subscriber>>,
    diagnostics: Arc<dyn DiagnosticSink>,
    limits: Limits,
    peer_version: PeerPin,
}

impl AgentClient<HttpTransport> {
    /// Starts configuring a client for the agent endpoint at `url`. Transport
    /// options (headers, timeouts, retry) and client options share one builder.
    pub fn http(url: impl Into<String>) -> ClientBuilder<HttpTransportBuilder> {
        ClientBuilder::new(HttpTransport::builder(url))
    }
}

impl<T: Transport> AgentClient<T> {
    /// Starts configuring a client over a custom transport.
    pub fn with_transport(transport: T) -> ClientBuilder<T> {
        ClientBuilder::new(transport)
    }

    /// Starts a run. Nothing is sent until the handle is polled or awaited.
    ///
    /// The input goes out with `protocolVersion` declared as `"1.0"` (omitted
    /// toward a peer pinned to a pre-versioning era, RUNIN-08) and activity
    /// messages removed (RUNIN-12); a request that fails the outgoing checks
    /// fails the run before anything is sent (PROC-32..PROC-34).
    ///
    /// Await the handle for the [`RunReport`], or call `next()` on it for each
    /// [`RunUpdate`] as it arrives. Here a canned [`Transport`] stands in for an
    /// agent:
    ///
    /// ```
    /// use agui_sdk::client::transport::{Transport, TransportRequest};
    /// use agui_sdk::client::{Frame, FrameStream};
    /// use serde_json::json;
    ///
    /// /// Answers every run with one assistant message, as an agent would.
    /// struct Canned;
    ///
    /// impl Transport for Canned {
    ///     async fn open(&self, req: TransportRequest) -> agui_sdk::Result<FrameStream> {
    ///         let (thread, run) = (req.thread_id.as_str(), req.run_id.as_str());
    ///         let id = format!("reply-{run}");
    ///         let events = [
    ///             json!({ "type": "RUN_STARTED", "threadId": thread, "runId": run }),
    ///             json!({ "type": "TEXT_MESSAGE_START", "messageId": id, "role": "assistant" }),
    ///             json!({ "type": "TEXT_MESSAGE_CONTENT", "messageId": id, "delta": "Hi!" }),
    ///             json!({ "type": "TEXT_MESSAGE_END", "messageId": id }),
    ///             json!({ "type": "RUN_FINISHED", "threadId": thread, "runId": run }),
    ///         ];
    ///         Ok(FrameStream::from_frames(events.map(|e| Ok(Frame::from_value(e)))))
    ///     }
    /// }
    /// use agui_sdk::client::{AgentClient, RunRequest};
    /// use agui_sdk::Event;
    ///
    /// # let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
    /// # rt.block_on(async {
    /// let client = AgentClient::with_transport(Canned).build()?;
    ///
    /// // Await the whole run...
    /// let report = client.run(RunRequest::new().user("Hello")).await?;
    /// assert!(report.status.is_success());
    /// assert_eq!(report.new_messages().count(), 1);
    ///
    /// // ...or watch it stream.
    /// let mut run = client.run(RunRequest::new().user("Hello again"));
    /// let mut text = String::new();
    /// while let Some(update) = run.next().await {
    ///     if let Event::TextMessageContent(e) = &*update?.event {
    ///         text.push_str(&e.delta);
    ///     }
    /// }
    /// assert_eq!(text, "Hi!");
    /// # Ok::<(), agui_sdk::Error>(())
    /// # }).unwrap();
    /// ```
    pub fn run(&self, request: RunRequest) -> RunHandle {
        let mut request = request;
        let (thread_id, run_id) = request.ids();
        let (input, headers, cancel) = request.into_parts();
        self.start(input, headers, cancel, thread_id, run_id)
    }

    /// A run of an already assembled input (used by [`Thread`]).
    fn start(
        &self,
        input: Result<RunAgentInput>,
        headers: Vec<transport::Header>,
        cancel: Option<CancelToken>,
        thread_id: ThreadId,
        run_id: RunId,
    ) -> RunHandle {
        let transport = Arc::clone(&self.transport);
        let open: run::Open =
            Box::new(move |request| Box::pin(async move { transport.open(request).await }));
        RunHandle::new(run::RunSetup {
            config: Arc::clone(&self.config),
            open,
            input,
            headers,
            cancel,
            thread_id,
            run_id,
        })
    }

    /// A new conversation with a fresh random thread id.
    #[must_use]
    pub fn thread(&self) -> Thread<T> {
        Thread::new(self.clone(), ThreadId::random())
    }

    /// A conversation with a known thread id (history can be restored with
    /// [`Thread::with_history`] or [`Thread::restore`]).
    #[must_use]
    pub fn thread_with_id(&self, id: impl Into<ThreadId>) -> Thread<T> {
        Thread::new(self.clone(), id)
    }

    /// The transport.
    #[must_use]
    pub fn transport(&self) -> &T {
        &self.transport
    }
}

impl<T> Clone for AgentClient<T> {
    fn clone(&self) -> Self {
        AgentClient {
            transport: Arc::clone(&self.transport),
            config: Arc::clone(&self.config),
        }
    }
}

impl<T> std::fmt::Debug for AgentClient<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AgentClient")
            .field("middleware", &self.config.middleware.len())
            .field("observers", &self.config.observers.len())
            .field("subscribers", &self.config.subscribers.len())
            .field("limits", &self.config.limits)
            .field("peer_version", &self.config.peer_version)
            .finish_non_exhaustive()
    }
}

/// Configures an [`AgentClient`]. Setters are infallible; validation happens
/// in `build`.
#[must_use = "a builder does nothing until `build` is called"]
pub struct ClientBuilder<T> {
    transport: T,
    middleware: Vec<Arc<dyn Middleware>>,
    observers: Vec<Arc<dyn Observer>>,
    subscribers: Vec<Arc<subscriber::Subscriber>>,
    diagnostics: Option<Arc<dyn DiagnosticSink>>,
    limits: Limits,
    peer_version: PeerPin,
}

impl<T> ClientBuilder<T> {
    fn new(transport: T) -> Self {
        ClientBuilder {
            transport,
            middleware: Vec::new(),
            observers: Vec::new(),
            subscribers: Vec::new(),
            diagnostics: None,
            limits: Limits::default(),
            peer_version: PeerPin::default(),
        }
    }

    /// Adds a middleware. The first added is outermost.
    pub fn middleware(mut self, middleware: impl Middleware) -> Self {
        self.middleware.push(Arc::new(middleware));
        self
    }

    /// Adds an observer.
    pub fn observer(mut self, observer: impl Observer + 'static) -> Self {
        self.observers.push(Arc::new(observer));
        self
    }

    /// Installs a legacy-style subscriber ([`subscriber::Subscriber`]).
    pub fn subscriber(mut self, subscriber: subscriber::Subscriber) -> Self {
        self.subscribers.push(Arc::new(subscriber));
        self
    }

    /// Where diagnostics go. Defaults to
    /// [`default_sink`](crate::diagnostics::default_sink), never silent
    /// (VER-11).
    pub fn diagnostic_sink(mut self, sink: impl DiagnosticSink + 'static) -> Self {
        self.diagnostics = Some(Arc::new(sink));
        self
    }

    /// Resource budgets for the pipeline.
    pub fn limits(mut self, limits: Limits) -> Self {
        self.limits = limits;
        self
    }

    /// Pins the peer to a known protocol version. A [`PeerPin::Legacy`] pin
    /// selects that era's shims and omits `protocolVersion` from input
    /// (RUNIN-08, VER-03, VER-30). Unset, the peer is treated as current
    /// (VER-31). Parse a `0.0.N` string with
    /// [`LegacyEra::parse`](crate::version::LegacyEra::parse).
    pub fn peer_version(mut self, pin: PeerPin) -> Self {
        self.peer_version = pin;
        self
    }

    fn into_parts(self) -> (T, ClientConfigDraft) {
        (
            self.transport,
            ClientConfigDraft {
                middleware: self.middleware,
                observers: self.observers,
                subscribers: self.subscribers,
                diagnostics: self.diagnostics,
                limits: self.limits,
                peer_version: self.peer_version,
            },
        )
    }

    fn map_transport(self, edit: impl FnOnce(T) -> T) -> Self {
        ClientBuilder {
            transport: edit(self.transport),
            ..self
        }
    }
}

/// The client options of a builder, before validation.
struct ClientConfigDraft {
    middleware: Vec<Arc<dyn Middleware>>,
    observers: Vec<Arc<dyn Observer>>,
    subscribers: Vec<Arc<subscriber::Subscriber>>,
    diagnostics: Option<Arc<dyn DiagnosticSink>>,
    limits: Limits,
    peer_version: PeerPin,
}

impl ClientConfigDraft {
    /// Validates the client options and attaches `transport`.
    fn finish<U>(self, transport: U) -> Result<AgentClient<U>> {
        check_limits(&self.limits)?;
        let diagnostics = self
            .diagnostics
            .unwrap_or_else(crate::diagnostics::default_sink);
        if let PeerPin::Version(version) = self.peer_version {
            // A pinned versioned peer: we still declare ours, and say once
            // how the pin relates to it (VER-21..VER-23, VER-26).
            let declared = version.to_string();
            let class = PeerVersion::classify(Some(&declared));
            if let Some(code) = class.diagnostic_code() {
                diagnostics.report(&Diagnostic::new(
                    code,
                    Scope::Stream,
                    "",
                    format!(
                        "the peer is pinned to protocol {declared}; this client speaks {}",
                        crate::PROTOCOL_VERSION
                    ),
                ));
            }
        }
        Ok(AgentClient {
            transport: Arc::new(transport),
            config: Arc::new(ClientConfig {
                middleware: self.middleware,
                observers: self.observers,
                subscribers: self.subscribers,
                diagnostics,
                limits: self.limits,
                peer_version: self.peer_version,
            }),
        })
    }
}

/// Zero budgets would refuse every event.
fn check_limits(limits: &Limits) -> Result<()> {
    let zero = [
        ("max_frame_bytes", limits.max_frame_bytes),
        ("max_container_entries", limits.max_container_entries),
        ("max_depth", limits.max_depth),
        (
            "max_diagnostics_per_event",
            limits.max_diagnostics_per_event,
        ),
        ("patch.max_depth", limits.patch.max_depth),
    ]
    .into_iter()
    .find(|(_, value)| *value == 0);
    match zero {
        Some((name, _)) => Err(Error::new(
            ErrorCode::InvalidConfig,
            format!("limit `{name}` must be greater than zero"),
        )),
        None => Ok(()),
    }
}

impl<T: Transport> ClientBuilder<T> {
    /// Finishes.
    ///
    /// # Errors
    ///
    /// [`ErrorCode::InvalidConfig`] for an
    /// invalid budget.
    pub fn build(self) -> crate::Result<AgentClient<T>> {
        let (transport, draft) = self.into_parts();
        draft.finish(transport)
    }
}

impl ClientBuilder<HttpTransportBuilder> {
    /// Adds a request header for every run. `Accept`, `Content-Type`, `Host`,
    /// `Content-Length`, `Transfer-Encoding` and `Connection` are reserved and
    /// rejected by `build` (the protocol and HTTP/1.1 framing set them,
    /// SSE-01, SSE-02). Per-run headers: [`RunRequest::header`].
    pub fn header(self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.map_transport(|t| t.header(name, value))
    }

    /// Adds a header whose value is redacted from `Debug` output.
    pub fn sensitive_header(self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.map_transport(|t| t.sensitive_header(name, value))
    }

    /// Sets `Authorization: Bearer <token>` (redacted).
    pub fn bearer_token(self, token: impl AsRef<str>) -> Self {
        self.map_transport(|t| t.bearer_token(token))
    }

    /// Edits the transport configuration directly.
    pub fn transport(
        self,
        edit: impl FnOnce(HttpTransportBuilder) -> HttpTransportBuilder,
    ) -> Self {
        self.map_transport(edit)
    }

    /// Finishes.
    ///
    /// # Errors
    ///
    /// [`ErrorCode::InvalidConfig`] for an
    /// invalid URL, a reserved header, an invalid budget, invalid TLS
    /// material, or an `https://` URL with neither `rustls` nor `native-tls`.
    pub fn build(self) -> crate::Result<AgentClient<HttpTransport>> {
        let (builder, draft) = self.into_parts();
        // Runs before `HttpTransportBuilder::build`, so a bad budget is
        // reported without touching TLS or proxy setup; `finish` checks again
        // for the generic path. Removing this changes which error comes first.
        check_limits(&draft.limits)?;
        draft.finish(builder.build()?)
    }
}

impl<T> std::fmt::Debug for ClientBuilder<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ClientBuilder")
            .field("middleware", &self.middleware.len())
            .field("observers", &self.observers.len())
            .field("subscribers", &self.subscribers.len())
            .field("custom_sink", &self.diagnostics.is_some())
            .field("limits", &self.limits)
            .field("peer_version", &self.peer_version)
            .finish_non_exhaustive()
    }
}
