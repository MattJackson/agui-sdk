//! Framework-independent HTTP handling for the agent endpoint.
//!
//! [`handle`] takes the parts of a request any framework can provide and
//! returns either a rejection (status, JSON error body; before any stream,
//! SSE-12) or a streaming response whose [`ResponseBody`] yields encoded
//! [`Chunk`]s. The framework glue writes them and, for SSE, interleaves
//! keep-alives on its own timer ([`ResponseBody::keepalive`]).
//! A failure after the stream opened travels in-stream as `RUN_ERROR`
//! (SSE-13). The body closes after the last terminal event (SSE-10).
//!
//! # The binding, for a custom transport (TRN-13, TRN-14)
//!
//! * **Input delivery.** One `POST` whose body is the `RunAgentInput` JSON,
//!   `Content-Type: application/json`, delivered whole before any event
//!   (TRN-02). There is no further input during the run (PAT-01, PAT-02).
//! * **Negotiation.** `Accept` chooses the response format: protobuf whenever
//!   the most specific range matching its media type has `q > 0` (relative
//!   weights against SSE are never compared, PB-03, PB-04), SSE otherwise,
//!   even when `Accept` names neither (PB-05: there is no `406`). Every
//!   response carries `Vary: Accept` (the glue adds it).
//! * **Admission errors** come before any stream, as an HTTP status with a
//!   JSON body `{"code": "...", "message": "..."}`:
//!   `415` (request not `application/json`), `413` (body over
//!   [`InputPolicy::max_body_bytes`](super::input::InputPolicy::max_body_bytes)),
//!   `400` (malformed input, an unsupported major version, or state that does
//!   not fit the agent). Unknown input members are stripped with a warning,
//!   not rejected (RUNIN-29, RUNIN-30, PROC-35, TRN-05).
//! * **Framing.** SSE: one event per `data:` line, LF line endings, comment
//!   keep-alives. Protobuf: 4-byte big-endian length, then one event message.
//!   [`encode::json::NdjsonEncoder`](super::encode::json::NdjsonEncoder)
//!   frames a custom JSON channel the same way, one event object per line
//!   (TRN-15).
//! * **Ordering and termination.** Events are written in emission order, and
//!   the body ends right after the requested run's terminal event
//!   (`RUN_FINISHED` or `RUN_ERROR`), so a consumer tells completion from
//!   truncation by whether a terminal arrived (TRN-01, TRN-04).
//! * **Disconnect.** Dropping the [`ResponseBody`] cancels the run.

use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;

use bytes::BytesMut;

use crate::capability::{AgentCapabilities, TransportCapabilities};
use crate::error::{Error, ErrorCode};
use crate::event::Event;
use crate::lifecycle::RunError;
use crate::wire::{Chunk, WireFormat, JSON_MEDIA_TYPE};

use super::encode::Encoder;
use super::input::InputPolicy;
use super::{Agent, CancelToken, Extensions, RunStream, Runner};

/// Coalescing stops once a chunk holds this many bytes, so one poll never
/// buffers an unbounded amount.
const MAX_CHUNK_BYTES: usize = 64 * 1024;

/// Initial capacity of a response's output buffer.
const INITIAL_BUFFER: usize = 8 * 1024;

/// The most a response keeps allocated between events: an event or chunk
/// larger than this is not held for the rest of the response.
const RETAIN_BYTES: usize = 256 * 1024;

/// Default SSE keep-alive interval.
const DEFAULT_KEEPALIVE: Duration = Duration::from_secs(15);

/// Endpoint settings.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct ServeConfig {
    /// Admission policy.
    pub input: InputPolicy,
    /// SSE keep-alive interval for the framework glue. Default 15 s; `None` disables.
    pub keepalive: Option<Duration>,
    /// Offer protobuf when the client accepts it (feature `proto`). Default on.
    pub protobuf: bool,
    /// Declaration served by [`capabilities_response`], if any. Its
    /// `transport.streaming` and `transport.httpBinary` are overwritten with
    /// what this endpoint actually serves (SSE always; protobuf when
    /// [`protobuf`](Self::protobuf) is on and feature `proto` is built), so
    /// that part of the declaration cannot be wrong (CAP-03).
    pub capabilities: Option<AgentCapabilities>,
}

impl Default for ServeConfig {
    fn default() -> Self {
        ServeConfig {
            input: InputPolicy::default(),
            keepalive: Some(DEFAULT_KEEPALIVE),
            protobuf: true,
            capabilities: None,
        }
    }
}

impl ServeConfig {
    /// Whether this endpoint serves protobuf.
    fn serves_protobuf(&self) -> bool {
        self.protobuf && cfg!(feature = "proto")
    }
}

/// The request parts the handler needs.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct HttpRequest<'a> {
    /// `Content-Type` header.
    pub content_type: Option<&'a str>,
    /// `Accept` header.
    pub accept: Option<&'a str>,
    /// The body.
    pub body: &'a [u8],
    /// Request-scoped data for the agent ([`RunContext::extensions`](super::RunContext::extensions)).
    pub extensions: Extensions,
}

impl<'a> HttpRequest<'a> {
    /// A request with no extensions.
    #[must_use]
    pub fn new(content_type: Option<&'a str>, accept: Option<&'a str>, body: &'a [u8]) -> Self {
        HttpRequest {
            content_type,
            accept,
            body,
            extensions: Extensions::default(),
        }
    }

    /// Attaches request-scoped data (principal, tenant, trace context).
    #[must_use]
    pub fn with_extensions(self, extensions: Extensions) -> Self {
        HttpRequest { extensions, ..self }
    }
}

/// A pre-stream refusal.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct HttpRejection {
    /// HTTP status: 400, 413 or 415.
    pub status: u16,
    /// The error code.
    pub code: ErrorCode,
    /// JSON error body (`{"code": "...", "message": "..."}`); no input echoed.
    pub body: String,
}

impl HttpRejection {
    fn new(status: u16, code: ErrorCode, message: &str) -> Self {
        HttpRejection {
            status,
            code,
            body: error_body(code, message),
        }
    }

    /// The rejection for an admission or runner error: its own HTTP status
    /// when it carries one, else the status its code implies.
    fn from_error(error: &Error) -> Self {
        let code = error.code();
        let status = error.http_status().unwrap_or(match code {
            ErrorCode::RequestTooLarge => 413,
            ErrorCode::UnsupportedMediaType => 415,
            // Never produced by this crate's handler (PB-05); the natural
            // status for an application-supplied code (see `ErrorCode`).
            ErrorCode::NotAcceptable => 406,
            _ => 400,
        });
        HttpRejection::new(status, code, error.message())
    }
}

fn error_body(code: ErrorCode, message: &str) -> String {
    json_error_body(code.as_str(), message)
}

/// The one error body shape of every transport: `{"code", "message"}`.
pub(super) fn json_error_body(code: &str, message: &str) -> String {
    serde_json::json!({ "code": code, "message": message }).to_string()
}

/// A streaming response.
///
/// `#[non_exhaustive]`: later minors may add response parts (headers, a
/// status) without a breaking change. Read the fields; only [`handle`] builds
/// one.
#[non_exhaustive]
#[must_use = "the run makes progress only while the body is polled; dropping it cancels the run"]
pub struct HttpResponse {
    /// `Content-Type` to send with status 200.
    pub content_type: &'static str,
    /// Encoded chunks.
    pub body: ResponseBody,
}

impl std::fmt::Debug for HttpResponse {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HttpResponse")
            .field("content_type", &self.content_type)
            .field("body", &self.body)
            .finish()
    }
}

/// The encoded response body. Each poll encodes every event that is ready
/// into one [`Chunk`] (coalesced), carved from a reused buffer without a
/// per-event allocation once the buffer has grown. Consume with
/// [`next`](Self::next) / [`poll_next`](Self::poll_next), or as a
/// `futures_core::Stream` with feature `stream`. Dropping it cancels the run
/// (a disconnected client).
#[must_use = "the run makes progress only while the body is polled; dropping it cancels the run"]
pub struct ResponseBody {
    events: RunStream,
    encoder: Encoder,
    /// Encoded bytes not yet handed out; chunks are split off its front, and
    /// its allocation is reclaimed once the consumer drops them.
    buffer: BytesMut,
    /// One event's encoding, copied into `buffer`.
    scratch: Vec<u8>,
    /// Cancelled when the body is dropped before the stream ended.
    cancel: CancelToken,
    /// The event stream ended (or failed): nothing more to poll.
    done: bool,
    /// The keep-alive interval of the [`ServeConfig`] that produced it.
    #[cfg_attr(not(feature = "axum"), allow(dead_code))]
    keepalive_every: Option<Duration>,
}

impl ResponseBody {
    fn new(
        events: RunStream,
        encoder: Encoder,
        cancel: CancelToken,
        keepalive_every: Option<Duration>,
    ) -> Self {
        ResponseBody {
            events,
            encoder,
            buffer: BytesMut::with_capacity(INITIAL_BUFFER),
            scratch: Vec::new(),
            cancel,
            done: false,
            keepalive_every,
        }
    }

    /// The keep-alive interval configured where this body was made.
    #[cfg(feature = "axum")]
    pub(crate) fn keepalive_every(&self) -> Option<Duration> {
        self.keepalive_every
    }

    /// The encoder, for inspection.
    #[must_use]
    pub fn encoder(&self) -> &Encoder {
        &self.encoder
    }

    /// A keep-alive chunk for this format (an SSE comment, SSE-09), or
    /// `None` when the format has none. For the glue's timer.
    #[must_use]
    pub fn keepalive(&self) -> Option<Chunk> {
        self.encoder.keepalive_bytes().map(Chunk::from_static)
    }

    /// Polls for the next chunk of encoded events.
    pub fn poll_next(&mut self, cx: &mut Context<'_>) -> Poll<Option<Chunk>> {
        while !self.done && self.buffer.len() < MAX_CHUNK_BYTES {
            match self.events.poll_next(cx) {
                Poll::Ready(Some(event)) => self.write(&event),
                Poll::Ready(None) => self.done = true,
                Poll::Pending => break,
            }
        }
        if !self.buffer.is_empty() {
            let chunk = self.buffer.split();
            if chunk.len() > RETAIN_BYTES {
                // Not reclaimed: its allocation is freed with the chunk.
                self.buffer = BytesMut::with_capacity(INITIAL_BUFFER);
            }
            return Poll::Ready(Some(Chunk::from_bytes(chunk.freeze())));
        }
        if self.done {
            Poll::Ready(None)
        } else {
            Poll::Pending
        }
    }

    /// Encodes one event into the output buffer. An event that cannot be
    /// encoded ends the stream with an in-stream `RUN_ERROR` (SSE-13): the
    /// run is over, its remaining events are not read, and the agent is
    /// cancelled.
    fn write(&mut self, event: &Event) {
        self.scratch.clear();
        match self.encoder.encode(event, &mut self.scratch) {
            Ok(()) => self.buffer.extend_from_slice(&self.scratch),
            Err(error) => {
                self.done = true;
                self.cancel.cancel();
                let failure = Event::RunError(
                    RunError::new(error.message().to_owned()).with_code(ErrorCode::Encode.as_str()),
                );
                self.scratch.clear();
                if self.encoder.encode(&failure, &mut self.scratch).is_ok() {
                    self.buffer.extend_from_slice(&self.scratch);
                }
            }
        }
        if self.scratch.capacity() > RETAIN_BYTES {
            self.scratch = Vec::new();
        }
    }

    /// The next chunk; `None` after the terminal event has been written.
    // An inherent async `next`, like an async iterator; not `Iterator::next`.
    #[allow(clippy::should_implement_trait)]
    pub fn next(&mut self) -> impl Future<Output = Option<Chunk>> + Send + '_ {
        NextChunk { body: self }
    }
}

impl Drop for ResponseBody {
    fn drop(&mut self) {
        if !self.done {
            self.cancel.cancel();
        }
    }
}

struct NextChunk<'a> {
    body: &'a mut ResponseBody,
}

impl Future for NextChunk<'_> {
    type Output = Option<Chunk>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Chunk>> {
        self.get_mut().body.poll_next(cx)
    }
}

#[cfg(feature = "stream")]
#[cfg_attr(docsrs, doc(cfg(feature = "stream")))]
impl futures_core::Stream for ResponseBody {
    type Item = Chunk;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Chunk>> {
        ResponseBody::poll_next(self.get_mut(), cx)
    }
}

impl std::fmt::Debug for ResponseBody {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ResponseBody")
            .field("encoder", &self.encoder)
            .field("buffered", &self.buffer.len())
            .field("done", &self.done)
            .finish_non_exhaustive()
    }
}

/// Whether `content_type` is `application/json` (parameters such as
/// `charset` ignored, compared ASCII case-insensitively).
fn is_json(content_type: &str) -> bool {
    crate::wire::essence(content_type).eq_ignore_ascii_case(JSON_MEDIA_TYPE)
}

/// Handles one POST: negotiate, check the media type (415), admit the
/// input (400/413), check the state fits the agent (400), then start the run.
///
/// ```
/// # use agui_sdk::server::{Agent, BoxError, RunContext, RunOutcome};
/// #
/// # struct Echo;
/// #
/// # impl Agent for Echo {
/// #     type State = serde_json::Value;
/// #
/// #     async fn run(&self, ctx: RunContext) -> Result<RunOutcome, BoxError> {
/// #         ctx.emitter().say("Hello").await?;
/// #         Ok(RunOutcome::success())
/// #     }
/// # }
/// use agui_sdk::server::http::{handle, HttpRequest, ServeConfig};
/// use agui_sdk::server::{CancelToken, Runner};
///
/// let runner = Runner::new(Echo);
/// let request = HttpRequest::new(
///     Some("application/json"),
///     Some("text/event-stream"),
///     br#"{"threadId":"t-1","runId":"r-1","messages":[]}"#,
/// );
/// let response = handle(&runner, &ServeConfig::default(), request, CancelToken::new())
///     .expect("a valid request");
/// assert_eq!(response.content_type, "text/event-stream");
///
/// // Write each chunk to the connection; here, collect them.
/// let mut body = response.body;
/// let mut sse = Vec::new();
/// futures_executor::block_on(async {
///     while let Some(chunk) = body.next().await {
///         sse.extend_from_slice(&chunk);
///     }
/// });
/// let sse = String::from_utf8(sse).unwrap();
/// assert!(sse.starts_with("data: {\"type\":\"RUN_STARTED\""));
/// assert!(sse.contains("RUN_FINISHED"));
/// ```
/// # Errors
///
/// The rejection to send instead of a stream.
pub fn handle<A: Agent>(
    runner: &Runner<A>,
    config: &ServeConfig,
    request: HttpRequest<'_>,
    cancel: CancelToken,
) -> Result<HttpResponse, HttpRejection> {
    let format = WireFormat::negotiate(request.accept, config.serves_protobuf());
    if !request.content_type.is_some_and(is_json) {
        return Err(HttpRejection::new(
            415,
            ErrorCode::UnsupportedMediaType,
            "the request body must be RunAgentInput as application/json",
        ));
    }
    // Admission enforces the body limit (413) before parsing anything.
    let admitted = super::input::admit(request.body, &config.input)
        .map_err(|error| HttpRejection::from_error(&error))?;
    let events = runner
        .run(admitted, cancel.clone(), request.extensions)
        .map_err(|error| HttpRejection::from_error(&error))?;
    let encoder = Encoder::for_format(format);
    Ok(HttpResponse {
        content_type: encoder.content_type(),
        body: ResponseBody::new(events, encoder, cancel, config.keepalive),
    })
}

/// The capability declaration as a JSON body, if configured (CAP-12).
/// Transport flags are derived from `config`, not taken from the author
/// (CAP-03).
#[must_use]
pub fn capabilities_response(config: &ServeConfig) -> Option<String> {
    let mut capabilities = config.capabilities.clone()?;
    let mut transport = capabilities
        .transport
        .take()
        .unwrap_or_else(TransportCapabilities::default);
    transport.streaming = Some(true);
    transport.http_binary = Some(config.serves_protobuf());
    capabilities.transport = Some(transport);
    serde_json::to_string(&capabilities).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_media_type_ignores_parameters_and_case() {
        assert!(is_json("application/json"));
        assert!(is_json("Application/JSON; charset=utf-8"));
        assert!(is_json(" application/json ;x=1"));
        assert!(!is_json("application/json-patch+json"));
        assert!(!is_json("text/plain"));
        assert!(!is_json(""));
    }

    struct Big;

    impl Agent for Big {
        type State = serde_json::Value;

        async fn run(
            &self,
            ctx: super::super::RunContext,
        ) -> Result<super::super::RunOutcome, super::super::BoxError> {
            ctx.emitter().say("x".repeat(4 * RETAIN_BYTES)).await?;
            ctx.emitter().say("small").await?;
            Ok(super::super::RunOutcome::success())
        }
    }

    #[test]
    fn one_large_event_is_not_retained_for_the_rest_of_the_response() {
        let runner = Runner::new(Big);
        let request = HttpRequest::new(
            Some("application/json"),
            Some("text/event-stream"),
            br#"{"threadId":"t-1","runId":"r-1","messages":[]}"#,
        );
        let Ok(response) = handle(
            &runner,
            &ServeConfig::default(),
            request,
            CancelToken::new(),
        ) else {
            panic!("a valid request")
        };
        let mut body = response.body;
        let mut total = 0;
        futures_executor::block_on(async {
            while let Some(chunk) = body.next().await {
                total += chunk.len();
            }
        });
        assert!(total > 4 * RETAIN_BYTES);
        assert!(
            body.scratch.capacity() <= RETAIN_BYTES,
            "scratch kept {} bytes",
            body.scratch.capacity()
        );
        // Writing again would reclaim the buffer's whole allocation.
        body.buffer.reserve(1);
        assert!(
            body.buffer.capacity() <= RETAIN_BYTES,
            "buffer kept {} bytes",
            body.buffer.capacity()
        );
    }

    #[test]
    fn rejection_bodies_are_json_with_code_and_message() {
        let r = HttpRejection::new(406, ErrorCode::NotAcceptable, "no \"format\"");
        let body: serde_json::Value = serde_json::from_str(&r.body).unwrap_or_default();
        assert_eq!(body["code"], "server.not_acceptable");
        assert_eq!(body["message"], "no \"format\"");
    }
}
