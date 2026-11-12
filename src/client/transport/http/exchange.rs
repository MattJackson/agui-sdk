//! One HTTP exchange: send, retry before headers, follow same-origin
//! redirects, turn rejections into errors with a bounded body, and stream the
//! accepted body into frames.

use std::future::Future;
use std::pin::{pin, Pin};
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use ::http::header::{
    HeaderMap, HeaderValue, ACCEPT, CONTENT_TYPE, LOCATION, PROXY_AUTHORIZATION, RETRY_AFTER,
};
use ::http::{Method, Request, Response, StatusCode, Uri};
use bytes::Bytes;
use http_body::Body as _;
use http_body_util::Full;
use hyper::body::Incoming;
use tokio::time::{Instant, Sleep};

use super::connect::{ConnectTimeout, NoTls};
use super::endpoint::{self, Origin};
use super::HttpInner;
use crate::cancel::{CancelToken, Cancelled};
use crate::client::middleware::{Frame, FrameSource, FrameStream};
use crate::client::transport::sse::SseDecoder;
use crate::client::transport::TransportRequest;
use crate::diagnostics::DiagnosticSink;
use crate::error::{Error, ErrorCode, Result};
use crate::wire::{Chunk, WireFormat, JSON_MEDIA_TYPE};

/// Far enough away to mean "never" without overflowing `Instant`.
pub(super) const NEVER: Duration = Duration::from_secs(60 * 60 * 24 * 365 * 30);

/// `delay` from now, with `Duration::MAX` (the crate's "off") and any other
/// delay too large for `Instant` clamped to [`NEVER`].
fn in_from_now(delay: Duration) -> Instant {
    Instant::now() + delay.min(NEVER)
}

/// What a request is for; decides which redirects may be followed.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Purpose {
    /// The run POST: only 307/308 keep the method and body.
    Run,
    /// A capability GET: any same-origin redirect is safe.
    Capabilities,
}

/// Why a wait ended early.
enum Interrupted {
    Cancelled,
    Deadline,
}

fn cancelled_error() -> Error {
    Error::new(ErrorCode::Cancelled, "the run was cancelled")
}

fn total_timeout_error(limit: Option<Duration>) -> Error {
    match limit {
        Some(limit) => Error::new(
            ErrorCode::Timeout,
            format!("the exchange exceeded its total timeout of {limit:?}"),
        ),
        None => Error::new(
            ErrorCode::Timeout,
            "the exchange exceeded its total timeout",
        ),
    }
}

/// Polls `work` until it completes, `cancel` fires or `until` passes.
async fn guarded<F: Future>(
    work: F,
    cancel: &CancelToken,
    until: Instant,
) -> std::result::Result<F::Output, Interrupted> {
    let mut work = pin!(work);
    let mut cancelled = cancel.cancelled();
    let mut deadline = pin!(tokio::time::sleep_until(until));
    std::future::poll_fn(|cx| {
        if Pin::new(&mut cancelled).poll(cx).is_ready() {
            return Poll::Ready(Err(Interrupted::Cancelled));
        }
        if let Poll::Ready(out) = work.as_mut().poll(cx) {
            return Poll::Ready(Ok(out));
        }
        if deadline.as_mut().poll(cx).is_ready() {
            return Poll::Ready(Err(Interrupted::Deadline));
        }
        Poll::Pending
    })
    .await
}

/// The exchange's limits in time.
struct Clock {
    /// The total deadline, or [`NEVER`] from now.
    deadline: Instant,
    total: Option<Duration>,
}

impl Clock {
    fn new(total: Option<Duration>) -> Self {
        let now = Instant::now();
        Clock {
            deadline: now + total.map_or(NEVER, |t| t.min(NEVER)),
            total,
        }
    }

    fn interrupted(&self, why: Interrupted) -> Error {
        match why {
            Interrupted::Cancelled => cancelled_error(),
            Interrupted::Deadline => total_timeout_error(self.total),
        }
    }
}

/// Maps a client error raised before response headers, and whether it
/// happened while connecting (nothing was sent, so a retry is safe): a
/// refused connection, or a TCP, proxy or TLS-handshake timeout.
fn request_error(error: hyper_util::client::legacy::Error) -> (Error, bool) {
    let mut source: Option<&(dyn std::error::Error + 'static)> = Some(&error);
    while let Some(e) = source {
        if e.is::<ConnectTimeout>() {
            return (
                Error::new(ErrorCode::Timeout, e.to_string())
                    .with_source(error)
                    .with_connect_phase(),
                true,
            );
        }
        if e.is::<NoTls>() {
            return (
                Error::new(ErrorCode::InvalidConfig, e.to_string()).with_source(error),
                false,
            );
        }
        source = e.source();
    }
    if error.is_connect() {
        (
            Error::new(ErrorCode::Connect, "connecting to the agent failed").with_source(error),
            true,
        )
    } else {
        (
            Error::new(
                ErrorCode::StreamRead,
                "the request failed before response headers arrived",
            )
            .with_source(error),
            false,
        )
    }
}

/// Whether the policy may retry a failure: connect errors and 429, 502,
/// 503 and 504, all before any response body was used.
fn retryable_status(status: StatusCode) -> bool {
    matches!(status.as_u16(), 429 | 502 | 503 | 504)
}

/// A random fraction of `d` in `[0, d]`, from std's per-process random
/// hasher keys and a counter (no RNG dependency).
fn jittered(d: Duration) -> Duration {
    use std::collections::hash_map::RandomState;
    use std::hash::{BuildHasher, Hasher};
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let mut h = RandomState::new().build_hasher();
    h.write_u64(COUNTER.fetch_add(1, Ordering::Relaxed));
    let fraction = (h.finish() >> 11) as f64 / (1u64 << 53) as f64;
    d.mul_f64(fraction)
}

/// The prepared request, re-sent on retry and redirect.
struct Outgoing {
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
    purpose: Purpose,
}

/// Sends `out`, retrying and following redirects per the transport's policy,
/// until a 2xx response head arrives.
async fn send(
    inner: &HttpInner,
    mut out: Outgoing,
    cancel: &CancelToken,
    clock: &Clock,
) -> Result<Response<Incoming>> {
    let origin: Origin = endpoint::origin(&out.uri);
    let mut redirects = 0u32;
    let mut attempt = 0u32;
    loop {
        if cancel.is_cancelled() {
            return Err(cancelled_error());
        }
        let mut request = Request::new(Full::new(out.body.clone()));
        *request.method_mut() = out.method.clone();
        *request.uri_mut() = out.uri.clone();
        *request.headers_mut() = out.headers.clone();
        // The response-header timeout runs inside the total deadline.
        let head_by = in_from_now(inner.response_timeout).min(clock.deadline);
        let sent = guarded(inner.client.request(request), cancel, head_by).await;
        let response = match sent {
            Err(Interrupted::Deadline) if head_by < clock.deadline => {
                return Err(Error::new(
                    ErrorCode::Timeout,
                    format!(
                        "no response headers within the {:?} response timeout",
                        inner.response_timeout
                    ),
                ));
            }
            Err(why) => return Err(clock.interrupted(why)),
            Ok(Err(e)) => {
                let (error, connecting) = request_error(e);
                if connecting && attempt < inner.retry.max_retries {
                    // A wait past the deadline reports this failure.
                    let Some(wake) = retry_wake(inner, attempt, None, clock) else {
                        return Err(error);
                    };
                    pause(wake, cancel, clock).await?;
                    attempt += 1;
                    continue;
                }
                return Err(error);
            }
            Ok(Ok(response)) => response,
        };
        let status = response.status();
        if status.is_success() {
            return Ok(response);
        }
        if status.is_redirection() {
            let follow = matches!(status.as_u16(), 307 | 308)
                || (out.purpose == Purpose::Capabilities && matches!(status.as_u16(), 301..=303));
            let target = response
                .headers()
                .get(LOCATION)
                .and_then(|v| v.to_str().ok())
                .and_then(|l| endpoint::resolve_location(&out.uri, l));
            if let Some(target) = target {
                // Never across origins: the credentials would follow.
                if follow && redirects < inner.max_redirects && endpoint::origin(&target) == origin
                {
                    redirects += 1;
                    out.uri = target;
                    continue;
                }
            }
            return Err(status_error(inner, out.purpose, response, cancel, clock).await);
        }
        if retryable_status(status) && attempt < inner.retry.max_retries {
            let retry_after = response
                .headers()
                .get(RETRY_AFTER)
                .and_then(|v| v.to_str().ok())
                .and_then(endpoint::parse_retry_after);
            if let Some(after) = retry_after.filter(|_| inner.retry.honor_retry_after) {
                if after > inner.retry.max_retry_after {
                    // Longer than we are willing to wait: report it.
                    return Err(status_error(inner, out.purpose, response, cancel, clock).await);
                }
            }
            // A wait past the deadline reports the status (with its
            // Retry-After and body), as without a policy.
            let Some(wake) = retry_wake(inner, attempt, retry_after, clock) else {
                return Err(status_error(inner, out.purpose, response, cancel, clock).await);
            };
            drop(response);
            pause(wake, cancel, clock).await?;
            attempt += 1;
            continue;
        }
        return Err(status_error(inner, out.purpose, response, cancel, clock).await);
    }
}

/// When retry number `attempt` may be sent; `None` when that is past the
/// total deadline.
fn retry_wake(
    inner: &HttpInner,
    attempt: u32,
    retry_after: Option<Duration>,
    clock: &Clock,
) -> Option<Instant> {
    let policy = &inner.retry;
    let delay = match retry_after.filter(|_| policy.honor_retry_after) {
        // `send` already reports a longer delay instead of retrying; this
        // clamp is defensive.
        Some(after) => after.min(policy.max_retry_after),
        None if policy.jitter => jittered(policy.backoff(attempt).min(NEVER)),
        None => policy.backoff(attempt),
    };
    let wake = in_from_now(delay);
    (wake <= clock.deadline).then_some(wake)
}

/// Waits until `wake` (from [`retry_wake`]) before a retry.
async fn pause(wake: Instant, cancel: &CancelToken, clock: &Clock) -> Result<()> {
    guarded(tokio::time::sleep_until(wake), cancel, clock.deadline)
        .await
        .map_err(|why| clock.interrupted(why))
}

/// The error body of a rejection: what the server said, lossily decoded,
/// cut at the configured budget. Reached through `Error::source`. Its
/// `Display` escapes control characters, as `Error`'s own does, so a logger
/// walking the source chain cannot be made to write forged lines or terminal
/// escapes (docs/DECISIONS.md D26).
#[derive(Debug)]
pub(crate) struct ErrorBody {
    pub(crate) text: String,
    pub(crate) truncated: bool,
}

impl std::fmt::Display for ErrorBody {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        crate::diagnostics::write_escaped(f, &self.text)?;
        if self.truncated {
            f.write_str(" [...]")?;
        }
        Ok(())
    }
}

impl std::error::Error for ErrorBody {}

/// Reads up to `limit` body bytes before `until`;
/// `(bytes, more_remained, read_error, interrupted)`. Stops on the time
/// limit or a cancel (`interrupted`); a read error ends the read too and is
/// returned (best-effort callers ignore it).
async fn read_bounded(
    body: &mut Incoming,
    limit: usize,
    cancel: &CancelToken,
    until: Instant,
) -> (Vec<u8>, bool, Option<hyper::Error>, bool) {
    let mut bytes = Vec::new();
    let mut more = false;
    let mut failure = None;
    let reading = async {
        while let Some(frame) = std::future::poll_fn(|cx| Pin::new(&mut *body).poll_frame(cx)).await
        {
            let frame = match frame {
                Ok(frame) => frame,
                Err(e) => {
                    failure = Some(e);
                    break;
                }
            };
            if let Ok(data) = frame.into_data() {
                let room = limit - bytes.len();
                if data.len() > room {
                    bytes.extend_from_slice(&data[..room]);
                    more = true;
                    break;
                }
                bytes.extend_from_slice(&data);
            }
        }
    };
    let interrupted = guarded(reading, cancel, until).await.is_err();
    (bytes, more, failure, interrupted)
}

/// A pre-stream rejection (SSE-12, PB-19): the status, `Retry-After`, and
/// the start of the error body.
async fn status_error(
    inner: &HttpInner,
    purpose: Purpose,
    response: Response<Incoming>,
    cancel: &CancelToken,
    clock: &Clock,
) -> Error {
    let status = response.status();
    let retry_after = response
        .headers()
        .get(RETRY_AFTER)
        .and_then(|v| v.to_str().ok())
        .and_then(endpoint::parse_retry_after);
    let mut body = response.into_body();
    let until = in_from_now(inner.limits.error_body_timeout).min(clock.deadline);
    let (bytes, more, failure, interrupted) =
        read_bounded(&mut body, inner.limits.max_error_body_bytes, cancel, until).await;
    // Cut off by the size budget, the time budget, a cancel or a read error.
    let truncated = more || failure.is_some() || interrupted;
    let reason = status.canonical_reason().unwrap_or("");
    let what = if status.is_redirection() {
        " (redirect not followed)"
    } else {
        ""
    };
    let message = match purpose {
        Purpose::Run => format!(
            "the agent answered HTTP {} {reason}{what} before any event stream",
            status.as_u16()
        ),
        Purpose::Capabilities => format!(
            "the capabilities endpoint answered HTTP {} {reason}{what}",
            status.as_u16()
        ),
    };
    let mut error = Error::new(ErrorCode::HttpStatus, message).with_http_status(status.as_u16());
    if let Some(after) = retry_after {
        error = error.with_retry_after(after);
    }
    if !bytes.is_empty() {
        error = error.with_source(ErrorBody {
            text: String::from_utf8_lossy(&bytes).into_owned(),
            truncated,
        });
    }
    error
}

/// The `Accept` value for the offered formats: SSE always, protobuf when
/// offered and compiled in, never a wildcard (SSE-02, PB-01, PB-02, PB-06).
fn accept_value(accept: &[WireFormat]) -> (HeaderValue, bool) {
    let proto = cfg!(feature = "proto") && accept.contains(&WireFormat::Protobuf);
    let proto_first = proto
        && accept.iter().position(|f| *f == WireFormat::Protobuf)
            < accept.iter().position(|f| *f == WireFormat::Sse);
    let value = match (proto, proto_first) {
        (false, _) => "text/event-stream",
        (true, false) => "text/event-stream, application/vnd.ag-ui.event+proto",
        (true, true) => "application/vnd.ag-ui.event+proto, text/event-stream",
    };
    (HeaderValue::from_static(value), proto)
}

/// Opens one run: POSTs the input and returns its frame stream.
pub(super) async fn open(inner: &Arc<HttpInner>, request: TransportRequest) -> Result<FrameStream> {
    let clock = Clock::new(inner.total_timeout);
    let TransportRequest {
        body,
        headers: run_headers,
        accept,
        cancel,
        diagnostics,
        max_frame_bytes,
        #[cfg(feature = "proto")]
        max_container_entries,
        ..
    } = request;
    let mut headers = inner.headers.clone();
    // A per-run header replaces the transport's values of the same name
    // (as `fetch` does), so per-tenant auth on a shared client sends one
    // `Authorization`; several per-run values of one name are all sent.
    let mut per_run = Vec::with_capacity(run_headers.len());
    for header in &run_headers {
        per_run.push(super::header_pair(header, ErrorCode::InvalidRequest)?);
    }
    for (name, _) in &per_run {
        headers.remove(name);
    }
    for (name, value) in per_run {
        headers.append(name, value);
    }
    let (accept, proto) = accept_value(&accept);
    headers.insert(ACCEPT, accept);
    headers.insert(CONTENT_TYPE, HeaderValue::from_static(JSON_MEDIA_TYPE));
    let uri = inner.endpoint.uri.clone();
    if let Some(auth) = inner.proxy_auth_for(&uri) {
        headers.insert(PROXY_AUTHORIZATION, auth);
    }
    let out = Outgoing {
        method: Method::POST,
        uri,
        headers,
        body: body.into_bytes(),
        purpose: Purpose::Run,
    };
    let response = send(inner, out, &cancel, &clock).await?;
    let format = response
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .and_then(WireFormat::from_content_type)
        .filter(|f| *f == WireFormat::Sse || proto);
    let decoder = match format {
        // The client's frame limit bounds what is buffered and decoded on
        // either wire, before any parse. Protobuf frames reach the client
        // already decoded, past its own frame guard, so the entry budget
        // also applies while they are decoded.
        Some(WireFormat::Sse) => Decoder::Sse(SseDecoder::new(max_frame_bytes)),
        #[cfg(feature = "proto")]
        Some(WireFormat::Protobuf) => Decoder::Proto(
            crate::proto::framing::FrameDecoder::new(max_frame_bytes),
            max_container_entries,
        ),
        _ => {
            return Err(Error::new(
                ErrorCode::UnsupportedContentType,
                "the response Content-Type is not an event stream this client reads",
            )
            .with_http_status(response.status().as_u16()));
        }
    };
    let now = Instant::now();
    let idle = inner.idle_timeout;
    Ok(FrameStream::new(BodySource {
        body: response.into_body(),
        decoder,
        cancel: cancel.clone(),
        cancelled: cancel.cancelled(),
        idle_limit: idle,
        last_read: now,
        idle_timer: tokio::time::sleep_until(now + idle.unwrap_or(NEVER)),
        total_limit: clock.total,
        total_timer: tokio::time::sleep_until(clock.deadline),
        deadline: clock.deadline,
        diagnostics,
        eof: false,
        done: false,
    }))
}

/// GETs a capability declaration and returns its body.
pub(super) async fn fetch(
    inner: &HttpInner,
    url: &str,
    extra: &[(String, String)],
    timeout: Duration,
    max_bytes: usize,
) -> Result<Vec<u8>> {
    let target = endpoint::Endpoint::parse(url, ErrorCode::InvalidRequest, "the capabilities URL")?;
    let clock = Clock::new(Some(timeout));
    let cancel = CancelToken::new();
    // The transport's own headers (its bearer token, URL credentials, API
    // keys) go only to the agent's origin, by the rule redirects follow:
    // elsewhere only the request's own headers are sent.
    let mut headers = if endpoint::origin(&target.uri) == endpoint::origin(&inner.endpoint.uri) {
        inner.headers.clone()
    } else {
        HeaderMap::new()
    };
    if let Some((user, pass)) = &target.userinfo {
        if let Some(auth) = endpoint::basic_auth(user, pass) {
            headers.insert(::http::header::AUTHORIZATION, auth);
        }
    }
    // The request's own headers override both, as per-run headers do in
    // `open`: each name replaces the values above, and several values of
    // one name are all sent.
    let mut own = Vec::with_capacity(extra.len());
    for (name, value) in extra {
        let header = super::super::Header::new(name.clone(), value.clone());
        own.push(super::header_pair(&header, ErrorCode::InvalidRequest)?);
    }
    for (name, _) in &own {
        headers.remove(name);
    }
    for (name, value) in own {
        headers.append(name, value);
    }
    headers.insert(ACCEPT, HeaderValue::from_static(JSON_MEDIA_TYPE));
    if let Some(auth) = inner.proxy_auth_for(&target.uri) {
        headers.insert(PROXY_AUTHORIZATION, auth);
    }
    let out = Outgoing {
        method: Method::GET,
        uri: target.uri,
        headers,
        body: Bytes::new(),
        purpose: Purpose::Capabilities,
    };
    let response = send(inner, out, &cancel, &clock).await?;
    let mut body = response.into_body();
    let (bytes, more, failure, _) =
        read_bounded(&mut body, max_bytes, &cancel, clock.deadline).await;
    if Instant::now() >= clock.deadline {
        return Err(total_timeout_error(clock.total));
    }
    if let Some(e) = failure {
        return Err(Error::new(
            ErrorCode::StreamRead,
            "reading the capability declaration failed",
        )
        .with_source(e));
    }
    if more {
        return Err(Error::new(
            ErrorCode::CapabilitiesRejected,
            format!("the capability declaration exceeds {max_bytes} bytes"),
        ));
    }
    Ok(bytes)
}

/// The body decoder chosen by the response `Content-Type` (PB-07).
enum Decoder {
    Sse(SseDecoder),
    #[cfg(feature = "proto")]
    Proto(crate::proto::framing::FrameDecoder, usize),
}

impl Decoder {
    fn push(&mut self, chunk: Chunk) {
        match self {
            Decoder::Sse(d) => d.push(chunk),
            #[cfg(feature = "proto")]
            Decoder::Proto(d, _) => d.push(chunk),
        }
    }

    fn has_partial(&self) -> bool {
        match self {
            Decoder::Sse(d) => d.has_partial(),
            #[cfg(feature = "proto")]
            Decoder::Proto(d, _) => d.has_partial(),
        }
    }

    /// The next complete frame. Protobuf frames are decoded to their JSON
    /// form; an unknown envelope arm is dropped with a warning (PB-17).
    #[cfg_attr(not(feature = "proto"), allow(unused_variables))]
    fn next(&mut self, diagnostics: &dyn DiagnosticSink) -> Result<Option<Frame>> {
        match self {
            Decoder::Sse(d) => Ok(d.next_event()?.map(Frame::from_json)),
            #[cfg(feature = "proto")]
            Decoder::Proto(d, max_entries) => {
                use crate::proto::decode::{decode_event_within, Decoded};
                while let Some(payload) = d.next_frame()? {
                    match decode_event_within(&payload, *max_entries)? {
                        Decoded::Event(value) => return Ok(Some(Frame::from_value(value))),
                        Decoded::UnknownArm { field_number } => {
                            diagnostics.report(&crate::diagnostics::Diagnostic::new(
                                crate::diagnostics::DiagnosticCode::UnknownProtobufArm,
                                crate::diagnostics::Scope::Event,
                                "",
                                format!(
                                    "dropped a protobuf event with unknown envelope field {field_number}"
                                ),
                            ));
                        }
                    }
                }
                Ok(None)
            }
        }
    }
}

/// Whether a body read error means the connection went away mid-body
/// (closed early, reset), rather than the peer sending garbage.
fn dropped(error: &hyper::Error) -> bool {
    if error.is_incomplete_message() {
        return true;
    }
    let mut source: Option<&(dyn std::error::Error + 'static)> = Some(error);
    while let Some(e) = source {
        if let Some(io) = e.downcast_ref::<std::io::Error>() {
            return matches!(
                io.kind(),
                std::io::ErrorKind::UnexpectedEof
                    | std::io::ErrorKind::ConnectionReset
                    | std::io::ErrorKind::ConnectionAborted
                    | std::io::ErrorKind::BrokenPipe
            );
        }
        source = e.source();
    }
    false
}

pin_project_lite::pin_project! {
    /// The frame source over an accepted response body (`B` is generic for
    /// the unit tests; the transport reads hyper's `Incoming`).
    struct BodySource<B> {
        body: B,
        decoder: Decoder,
        cancel: CancelToken,
        cancelled: Cancelled,
        idle_limit: Option<Duration>,
        last_read: Instant,
        #[pin]
        idle_timer: Sleep,
        total_limit: Option<Duration>,
        #[pin]
        total_timer: Sleep,
        deadline: Instant,
        diagnostics: Arc<dyn DiagnosticSink>,
        eof: bool,
        done: bool,
    }
}

impl<B> FrameSource for BodySource<B>
where
    B: http_body::Body<Data = Bytes, Error = hyper::Error> + Unpin + Send + 'static,
{
    fn poll_frame(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Result<Frame>>> {
        let mut this = self.project();
        if *this.done {
            return Poll::Ready(None);
        }
        let fail = |done: &mut bool, error: Error| {
            *done = true;
            Poll::Ready(Some(Err(error)))
        };
        // Body chunks read in this call without a frame coming of them: a
        // peer that keeps the body ready with comments, ignored fields or
        // dropped protobuf arms yields to the executor after
        // `FRAMES_PER_POLL` of them.
        let mut chunks = 0usize;
        loop {
            if this.cancel.is_cancelled() {
                return fail(this.done, cancelled_error());
            }
            match this.decoder.next(&**this.diagnostics) {
                Ok(Some(frame)) => return Poll::Ready(Some(Ok(frame))),
                Ok(None) => {}
                Err(e) => return fail(this.done, e),
            }
            if chunks == crate::client::run::FRAMES_PER_POLL {
                cx.waker().wake_by_ref();
                return Poll::Pending;
            }
            if *this.eof {
                *this.done = true;
                if this.decoder.has_partial() {
                    return Poll::Ready(Some(Err(Error::new(
                        ErrorCode::Truncated,
                        "the response body ended inside an event",
                    ))));
                }
                return Poll::Ready(None);
            }
            match Pin::new(&mut *this.body).poll_frame(cx) {
                Poll::Ready(Some(Ok(frame))) => {
                    if let Ok(data) = frame.into_data() {
                        if !data.is_empty() {
                            let now = Instant::now();
                            *this.last_read = now;
                            if now >= *this.deadline {
                                return fail(this.done, total_timeout_error(*this.total_limit));
                            }
                            this.decoder.push(Chunk::from_bytes(data));
                        }
                    }
                    chunks += 1;
                    continue;
                }
                Poll::Ready(Some(Err(e))) => {
                    if dropped(&e) {
                        // The connection dropped: what was decoded stands,
                        // and the pipeline judges the missing terminal event
                        // (SSE-15, PB-10).
                        *this.eof = true;
                        continue;
                    }
                    return fail(
                        this.done,
                        Error::new(ErrorCode::StreamRead, "reading the response body failed")
                            .with_source(e),
                    );
                }
                Poll::Ready(None) => {
                    *this.eof = true;
                    continue;
                }
                Poll::Pending => {}
            }
            if Pin::new(&mut *this.cancelled).poll(cx).is_ready() {
                return fail(this.done, cancelled_error());
            }
            if this.total_timer.as_mut().poll(cx).is_ready() {
                return fail(this.done, total_timeout_error(*this.total_limit));
            }
            if let Some(limit) = *this.idle_limit {
                if this.idle_timer.as_mut().poll(cx).is_ready() {
                    let due = *this.last_read + limit;
                    if Instant::now() >= due {
                        return fail(
                            this.done,
                            Error::new(
                                ErrorCode::IdleTimeout,
                                format!("no bytes arrived for {limit:?}"),
                            ),
                        );
                    }
                    // Data arrived since the timer was set: re-arm it at the
                    // new due time (no allocation, no per-chunk timer work).
                    this.idle_timer.as_mut().reset(due);
                    continue;
                }
            }
            return Poll::Pending;
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::diagnostics::NullSink;

    /// A body that always has another chunk ready, then ends: a fast peer
    /// keeping hyper's one-chunk slot refilled.
    struct Flood(usize);

    impl http_body::Body for Flood {
        type Data = Bytes;
        type Error = hyper::Error;

        fn poll_frame(
            mut self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
        ) -> Poll<Option<std::result::Result<http_body::Frame<Bytes>, hyper::Error>>> {
            if self.0 == 0 {
                return Poll::Ready(None);
            }
            self.0 -= 1;
            Poll::Ready(Some(Ok(http_body::Frame::data(Bytes::from_static(
                b": keep-alive\n",
            )))))
        }
    }

    struct Noop;

    impl std::task::Wake for Noop {
        fn wake(self: Arc<Self>) {}
    }

    #[test]
    fn a_flood_of_chunks_without_frames_yields_to_the_executor() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap();
        let _entered = runtime.enter();
        let cancel = CancelToken::new();
        let now = Instant::now();
        let mut source = Box::pin(BodySource {
            body: Flood(100_000),
            decoder: Decoder::Sse(SseDecoder::new(1024)),
            cancelled: cancel.cancelled(),
            cancel,
            idle_limit: None,
            last_read: now,
            idle_timer: tokio::time::sleep_until(now + NEVER),
            total_limit: None,
            total_timer: tokio::time::sleep_until(now + NEVER),
            deadline: now + NEVER,
            diagnostics: Arc::new(NullSink),
            eof: false,
            done: false,
        });
        let waker = std::task::Waker::from(Arc::new(Noop));
        let mut cx = Context::from_waker(&waker);
        assert!(
            source.as_mut().poll_frame(&mut cx).is_pending(),
            "one poll read the whole flood without yielding"
        );
    }
}
