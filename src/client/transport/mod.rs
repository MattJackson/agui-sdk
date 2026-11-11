//! Transports: how input reaches an agent and frames come back.
//!
//! A [`Transport`] sends the serialized input and yields raw event
//! [`Frame`]s; it never yields typed events, so every transport's output goes
//! through the same compatibility boundary, middleware and enforcement
//! (PROC-06, PROC-07, TRN-13). [`HttpTransport`] implements the two HTTP
//! bindings; a custom transport (WebSocket, message bus, pipe, TRN-12)
//! implements the trait and should document its framing, input delivery,
//! termination and error signals (TRN-14), framing JSON events one per frame
//! as SSE does (TRN-15; [`sse::SseDecoder`] is reusable for that).
//!
//! The binding contract a transport must keep: ordered, complete delivery
//! (TRN-01); input before events (TRN-02); a termination signal distinct from
//! truncation, which here is the end of the frame stream versus an
//! `Err(stream.truncated)` or an early end (TRN-04); and pre-stream refusal as
//! an `Err` from [`Transport::open`], never as frames (TRN-05).

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use crate::cancel::CancelToken;
use crate::diagnostics::DiagnosticSink;
use crate::error::Result;
use crate::ids::{RunId, ThreadId};
use crate::wire::{Chunk, WireFormat};

use super::middleware::{Frame, FrameStream};

mod http;

/// Header names the protocol and HTTP/1.1 framing own (SSE-01, SSE-02); never
/// taken from callers.
pub(in crate::client) const RESERVED_HEADERS: [&str; 6] = [
    "accept",
    "content-type",
    "host",
    "content-length",
    "transfer-encoding",
    "connection",
];
pub(crate) mod input;
pub mod sse;

pub use http::{
    HttpLimits, HttpTransport, HttpTransportBuilder, RetryPolicy, TlsBackend, TlsRoots,
};

pub(crate) use http::redact_url;

/// One request header. `Debug` redacts the value when `sensitive`, and
/// unless the name is a known-safe one (`User-Agent`, `Accept-Language`,
/// request-id and trace-context headers, and a few more).
#[derive(Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Header {
    /// Header name, as given.
    pub name: String,
    /// Header value.
    pub value: String,
    /// Redact the value from `Debug` and diagnostics.
    pub sensitive: bool,
}

impl Header {
    /// A header.
    pub fn new(name: impl Into<String>, value: impl Into<String>) -> Self {
        Header {
            name: name.into(),
            value: value.into(),
            sensitive: false,
        }
    }

    /// A header whose value is redacted.
    pub fn sensitive(name: impl Into<String>, value: impl Into<String>) -> Self {
        Header {
            name: name.into(),
            value: value.into(),
            sensitive: true,
        }
    }

    /// Whether `Debug` output hides the value: marked sensitive, or a name
    /// not on the known-safe list.
    pub(crate) fn redacted(&self) -> bool {
        self.sensitive || !http::is_safe_name(&self.name)
    }
}

impl std::fmt::Debug for Header {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let value: &dyn std::fmt::Debug = if self.redacted() {
            &"<redacted>"
        } else {
            &self.value
        };
        f.debug_struct("Header")
            .field("name", &self.name)
            .field("value", value)
            .field("sensitive", &self.sensitive)
            .finish()
    }
}

/// What a transport is asked to send.
#[derive(Clone)]
#[non_exhaustive]
pub struct TransportRequest {
    /// The run input, serialized once by the client (`application/json`).
    pub body: Chunk,
    /// The input's thread id, for transports that route by thread (TRN-12).
    pub thread_id: ThreadId,
    /// The input's run id.
    pub run_id: RunId,
    /// Per-run headers ([`RunRequest::header`](crate::client::RunRequest::header)),
    /// sent with the transport's own; `HttpTransport` lets a per-run header
    /// replace the transport's headers of the same name.
    pub headers: Vec<Header>,
    /// Wire formats the client can read, in preference order; SSE is always
    /// included (PB-06).
    pub accept: Vec<WireFormat>,
    /// Cancellation; a transport closes its connection when cancelled, and
    /// can await [`CancelToken::cancelled`] alongside its I/O.
    pub cancel: CancelToken,
    /// Where transport-level warnings go (e.g. an unknown protobuf arm).
    pub diagnostics: Arc<dyn DiagnosticSink>,
    /// Largest event frame to buffer and decode (SSE event data, or a
    /// protobuf frame's payload): the client's
    /// [`Limits::max_frame_bytes`](crate::pipeline::Limits::max_frame_bytes).
    /// A transport refuses a larger frame with
    /// [`ErrorCode::FrameTooLarge`](crate::ErrorCode::FrameTooLarge) before
    /// decoding it.
    pub max_frame_bytes: usize,
    /// Most container entries (object members plus array elements) one
    /// decoded event may hold: the client's
    /// [`Limits::max_container_entries`](crate::pipeline::Limits::max_container_entries).
    /// A transport that decodes a binary wire itself stops materialising an
    /// event that exceeds it, so a small frame cannot expand past what the
    /// pipeline would accept.
    pub max_container_entries: usize,
}

impl TransportRequest {
    /// A request with no per-run headers, a fresh cancel token, the
    /// [`default_sink`](crate::diagnostics::default_sink), the default frame
    /// limit and the `Accept` list this build supports. For custom transports' tests; set the public
    /// fields to vary the rest.
    pub fn new(body: Chunk, thread_id: ThreadId, run_id: RunId) -> Self {
        let mut accept = vec![WireFormat::Sse];
        if cfg!(feature = "proto") {
            accept.push(WireFormat::Protobuf);
        }
        TransportRequest {
            body,
            thread_id,
            run_id,
            headers: Vec::new(),
            accept,
            cancel: CancelToken::new(),
            diagnostics: crate::diagnostics::default_sink(),
            max_frame_bytes: crate::pipeline::Limits::default().max_frame_bytes,
            max_container_entries: crate::pipeline::Limits::default().max_container_entries,
        }
    }
}

impl std::fmt::Debug for TransportRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TransportRequest")
            .field("body", &self.body)
            .field("thread_id", &self.thread_id)
            .field("run_id", &self.run_id)
            .field("headers", &self.headers)
            .field("accept", &self.accept)
            .field("cancel", &self.cancel)
            .field("max_frame_bytes", &self.max_frame_bytes)
            .field("max_container_entries", &self.max_container_entries)
            .finish_non_exhaustive()
    }
}

/// Sends run input and yields event frames.
///
/// `open` resolves once the peer has accepted the request (all earlier
/// failures, including HTTP error statuses, are its `Err`, and it is the only
/// point where retrying is safe). The returned stream yields frames in
/// arrival order, ends cleanly at the end of the body, and yields one final
/// `Err` for transport failures; it ends early (and the pipeline reports
/// truncation) when the connection drops (SSE-15, PB-10).
///
/// A custom transport documents four things (TRN-14); this is the contract
/// the pipeline relies on (TRN-13):
///
/// # Framing
///
/// One frame per protocol event, in emission order, nothing dropped or
/// merged (TRN-01). A JSON transport frames each event as one JSON object,
/// as the SSE binding's `data` field does (TRN-15): build frames with
/// [`json_frame`], or reuse [`sse::SseDecoder`] on a byte channel.
///
/// # Input delivery
///
/// [`TransportRequest::body`] is the whole `RunAgentInput`, delivered to
/// the agent before any event is read (TRN-02), exactly once per exchange
/// (PAT-01). Nothing else is sent during the run.
///
/// # Termination
///
/// The end of the frame stream (`None`) means the channel closed normally;
/// whether the run completed is judged from its events, never from the
/// channel (TRN-04, SSE-14). A channel that broke mid-frame ends with
/// `Err` of code `stream.truncated`; one that closed between frames just
/// ends, and the missing terminal event makes it a truncated run (TRN-09).
///
/// # Errors
///
/// A refusal before the run starts (rejected input, authentication, an
/// unsupported format) is an `Err` from `open`, never frames (TRN-05). A
/// failure while streaming is the stream's final `Err`. Cancellation
/// ([`TransportRequest::cancel`]) ends either with `run.cancelled`.
/// A panic while opening or reading fails the run with
/// `transport.stream_read` ("the transport panicked"), with or without
/// middleware; it does not unwind into the caller.
///
/// Uses return-position `impl Trait` so implementations write `async fn`
/// and the future is `Send`; [`DynTransport`] is the object-safe twin.
/// Methods added to this trait in later minors will have default bodies.
pub trait Transport: Send + Sync + 'static {
    /// Opens one exchange.
    fn open(&self, request: TransportRequest) -> impl Future<Output = Result<FrameStream>> + Send;
}

mod sealed {
    pub trait Sealed {}
    impl<T: super::Transport> Sealed for T {}
}

/// Object-safe form of [`Transport`], blanket-implemented for every
/// transport. Sealed: implement [`Transport`] instead.
pub trait DynTransport: sealed::Sealed + Send + Sync + 'static {
    /// [`Transport::open`], boxed.
    fn open_boxed(
        &self,
        request: TransportRequest,
    ) -> Pin<Box<dyn Future<Output = Result<FrameStream>> + Send + '_>>;
}

impl<T: Transport> DynTransport for T {
    fn open_boxed(
        &self,
        request: TransportRequest,
    ) -> Pin<Box<dyn Future<Output = Result<FrameStream>> + Send + '_>> {
        Box::pin(self.open(request))
    }
}

/// A shared, type-erased transport. Implements [`Transport`], so
/// `AgentClient<BoxTransport>` works when the concrete type must be hidden.
pub type BoxTransport = Arc<dyn DynTransport>;

impl Transport for BoxTransport {
    async fn open(&self, request: TransportRequest) -> Result<FrameStream> {
        self.as_ref().open_boxed(request).await
    }
}

/// Helper for transports: a frame from a JSON event payload.
#[must_use]
pub fn json_frame(payload: Chunk) -> Frame {
    Frame::from_json(payload)
}
