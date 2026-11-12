//! Middleware: the client pipeline's mutation extension point, plus the
//! [`Frame`] and [`FrameStream`] types transports produce.
//!
//! A middleware sees the input before it is sent and every event frame
//! before enforcement, as raw JSON, unknown material included (PROC-21,
//! PROC-29, ARCH-02). It can rewrite, drop, insert or delay frames. It runs
//! after the compatibility boundary and era shims, so it always sees 1.0
//! shapes (VER-14), and it must handle both chunk and expanded spellings
//! (PROC-24): expansion happens later.
//!
//! The first registered middleware is outermost. A middleware is called once
//! per run and returns a [`FrameStream`]; per-frame work happens inside that
//! stream. [`map_frames`] covers the common per-frame case without writing a
//! stream by hand, and closures `Fn(MiddlewareRequest, Next) -> FrameStream`
//! are middleware.
//!
//! # Cost
//!
//! Frames reach middleware as [`Frame`]s. With middleware installed, the
//! innermost stage parses each transport frame once into a `Value` (the
//! compatibility boundary upgrades it there), so the first middleware already
//! sees a parsed frame; a pass-through middleware, or one that only peeks at
//! [`Frame::event_type`], adds no further parse. A frame a middleware builds
//! with [`Frame::from_json`] is parsed only when one asks
//! ([`Frame::value_mut`]). With no middleware installed the client skips this
//! module entirely and the pipeline parses each frame once, straight into
//! typed events.

use std::borrow::Cow;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use serde_json::Value;

use crate::diagnostics::DiagnosticSink;
use crate::error::{Error, ErrorCode, Result};
use crate::event::EventType;
use crate::wire::Chunk;

/// One event frame on its way through middleware.
#[derive(Debug, Clone)]
pub struct Frame {
    repr: FrameRepr,
}

#[derive(Debug, Clone)]
enum FrameRepr {
    Json(Chunk),
    Value(Value),
}

impl Frame {
    /// A frame from raw JSON bytes (one event object). No parse yet.
    pub fn from_json(json: Chunk) -> Self {
        Frame {
            repr: FrameRepr::Json(json),
        }
    }

    /// A frame from a JSON value (e.g. an inserted event).
    #[must_use]
    pub fn from_value(value: Value) -> Self {
        Frame {
            repr: FrameRepr::Value(value),
        }
    }

    /// The event's type, when it is a known one. Found by a scan of the
    /// bytes, without a full parse; falls back to parsing when the scan is
    /// inconclusive (escapes, `type` after a nested object). `None` for an
    /// unknown type or a frame with no string `type`.
    #[must_use]
    pub fn event_type(&self) -> Option<EventType> {
        self.raw_type().and_then(|t| EventType::parse(&t))
    }

    /// The raw `type` text, known or not. Borrowed unless the JSON string
    /// contains escapes.
    #[must_use]
    pub fn raw_type(&self) -> Option<Cow<'_, str>> {
        match &self.repr {
            FrameRepr::Value(value) => value.get("type").and_then(Value::as_str).map(Cow::Borrowed),
            FrameRepr::Json(bytes) => match scan_type(bytes) {
                Scan::Found(text) => Some(Cow::Borrowed(text)),
                Scan::Absent => None,
                Scan::Inconclusive => parse_type(bytes),
            },
        }
    }

    /// The frame as a JSON value, parsing it on first access.
    ///
    /// # Errors
    ///
    /// [`ErrorCode::MalformedEvent`] if the bytes are not a JSON object.
    pub fn value(&mut self) -> Result<&Value> {
        self.value_mut().map(|value| &*value)
    }

    /// Mutable access, parsing on first access.
    ///
    /// # Errors
    ///
    /// As [`value`](Self::value).
    pub fn value_mut(&mut self) -> Result<&mut Value> {
        if let FrameRepr::Json(bytes) = &self.repr {
            let value = parse_object(bytes)?;
            self.repr = FrameRepr::Value(value);
        }
        match &mut self.repr {
            FrameRepr::Value(value) => Ok(value),
            // Replaced just above.
            FrameRepr::Json(_) => Err(Error::new(ErrorCode::Internal, "frame was not parsed")),
        }
    }

    /// Takes the value, parsing if needed.
    ///
    /// # Errors
    ///
    /// As [`value`](Self::value).
    pub fn into_value(self) -> Result<Value> {
        match self.repr {
            FrameRepr::Json(bytes) => parse_object(&bytes),
            FrameRepr::Value(value) => Ok(value),
        }
    }

    /// The frame's size in bytes when it is still raw JSON (`None` once
    /// parsed). Used by the client's frame guard.
    pub(crate) fn raw_len(&self) -> Option<usize> {
        match &self.repr {
            FrameRepr::Json(bytes) => Some(bytes.len()),
            FrameRepr::Value(_) => None,
        }
    }

    /// The raw JSON bytes, when the frame was never parsed.
    pub(crate) fn as_json(&self) -> Option<&Chunk> {
        match &self.repr {
            FrameRepr::Json(bytes) => Some(bytes),
            FrameRepr::Value(_) => None,
        }
    }
}

fn malformed(message: &str) -> Error {
    Error::new(ErrorCode::MalformedEvent, message)
}

fn parse_object(bytes: &[u8]) -> Result<Value> {
    match serde_json::from_slice::<Value>(bytes) {
        Ok(value @ Value::Object(_)) => Ok(value),
        Ok(_) => Err(malformed("event frame is not a JSON object")),
        Err(err) => Err(malformed("event frame is not valid JSON").with_source(err)),
    }
}

/// Result of the no-parse scan for the top-level `type` member.
enum Scan<'a> {
    Found(&'a str),
    Absent,
    Inconclusive,
}

/// Looks for the top-level `"type"` member without parsing the frame. Only
/// flat members (strings, numbers, literals) are skipped; anything harder
/// (an escape in a key or in the value, a nested container before `type`,
/// malformed text) makes the scan inconclusive and the caller parses.
fn scan_type(bytes: &[u8]) -> Scan<'_> {
    let mut i = skip_ws(bytes, 0);
    if bytes.get(i) != Some(&b'{') {
        return Scan::Inconclusive;
    }
    i = skip_ws(bytes, i + 1);
    if bytes.get(i) == Some(&b'}') {
        return Scan::Absent;
    }
    loop {
        let Some((key, after_key)) = plain_string(bytes, i) else {
            return Scan::Inconclusive;
        };
        i = skip_ws(bytes, after_key);
        if bytes.get(i) != Some(&b':') {
            return Scan::Inconclusive;
        }
        i = skip_ws(bytes, i + 1);
        if key == b"type" {
            return match plain_string(bytes, i) {
                Some((value, _)) => match std::str::from_utf8(value) {
                    Ok(text) => Scan::Found(text),
                    Err(_) => Scan::Inconclusive,
                },
                // An escaped or non-string `type`: let the parser decide.
                None => Scan::Inconclusive,
            };
        }
        match bytes.get(i) {
            Some(b'"') => match plain_string(bytes, i) {
                Some((_, end)) => i = end,
                None => return Scan::Inconclusive,
            },
            Some(b'{' | b'[') | None => return Scan::Inconclusive,
            Some(_) => {
                while let Some(&b) = bytes.get(i) {
                    if b == b',' || b == b'}' || is_ws(b) {
                        break;
                    }
                    i += 1;
                }
            }
        }
        i = skip_ws(bytes, i);
        match bytes.get(i) {
            Some(b',') => i = skip_ws(bytes, i + 1),
            Some(b'}') => return Scan::Absent,
            _ => return Scan::Inconclusive,
        }
    }
}

fn is_ws(b: u8) -> bool {
    matches!(b, b' ' | b'\t' | b'\n' | b'\r')
}

fn skip_ws(bytes: &[u8], mut i: usize) -> usize {
    while bytes.get(i).copied().is_some_and(is_ws) {
        i += 1;
    }
    i
}

/// A JSON string at `start` with no escapes: its contents and the index
/// after the closing quote.
fn plain_string(bytes: &[u8], start: usize) -> Option<(&[u8], usize)> {
    if bytes.get(start) != Some(&b'"') {
        return None;
    }
    let body = bytes.get(start + 1..)?;
    let end = body.iter().position(|&b| b == b'"' || b == b'\\')?;
    if body[end] != b'"' {
        return None;
    }
    Some((&body[..end], start + 1 + end + 1))
}

/// The parsing fallback of [`Frame::raw_type`]: deserializes only `type`.
fn parse_type(bytes: &[u8]) -> Option<Cow<'_, str>> {
    #[derive(serde::Deserialize)]
    struct TypeOnly<'a> {
        #[serde(rename = "type", borrow, default)]
        kind: Option<Cow<'a, str>>,
    }
    serde_json::from_slice::<TypeOnly<'_>>(bytes)
        .ok()
        .and_then(|t| t.kind)
}

/// A source of frames: what a transport or middleware implements to produce
/// a [`FrameStream`]. Same contract as an async iterator: `Ready(None)` ends
/// the stream, a final `Ready(Some(Err(_)))` reports a failure.
pub trait FrameSource: Send + 'static {
    /// Polls for the next frame.
    fn poll_frame(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Result<Frame>>>;
}

/// A stream of frames. Boxed once per run, not per frame.
///
/// Consume it with [`next`](Self::next) (or [`poll_next`](Self::poll_next));
/// build one from a [`FrameSource`], a poll closure, an iterator, or (feature
/// `stream`) any `futures_core::Stream`. With feature `stream` it also
/// implements `futures_core::Stream`.
pub struct FrameStream {
    inner: Pin<Box<dyn FrameSource>>,
}

impl FrameStream {
    /// A stream over `source`.
    pub fn new(source: impl FrameSource) -> Self {
        FrameStream {
            inner: Box::pin(source),
        }
    }

    /// A stream driven by a poll closure.
    pub fn from_poll_fn(
        poll: impl FnMut(&mut Context<'_>) -> Poll<Option<Result<Frame>>> + Send + 'static,
    ) -> Self {
        FrameStream::new(PollFn {
            poll: Box::new(poll),
        })
    }

    /// A stream yielding `frames` then ending (for tests and replay).
    pub fn from_frames<I>(frames: I) -> Self
    where
        I: IntoIterator<Item = Result<Frame>>,
        I::IntoIter: Send + 'static,
    {
        FrameStream::new(Frames {
            frames: Box::new(frames.into_iter()),
        })
    }

    /// An empty stream.
    #[must_use]
    pub fn empty() -> Self {
        FrameStream::from_frames(std::iter::empty())
    }

    /// A stream over any `futures_core::Stream` of frames.
    #[cfg(feature = "stream")]
    #[cfg_attr(docsrs, doc(cfg(feature = "stream")))]
    pub fn from_stream<S>(stream: S) -> Self
    where
        S: futures_core::Stream<Item = Result<Frame>> + Send + 'static,
    {
        FrameStream::new(FromStream {
            stream: Box::pin(stream),
        })
    }

    /// Polls for the next frame.
    pub fn poll_next(&mut self, cx: &mut Context<'_>) -> Poll<Option<Result<Frame>>> {
        self.inner.as_mut().poll_frame(cx)
    }

    /// The next frame; `None` at the end of the stream.
    // An inherent async `next`, like an async iterator; not `Iterator::next`.
    #[allow(clippy::should_implement_trait)]
    pub fn next(&mut self) -> impl Future<Output = Option<Result<Frame>>> + Send + '_ {
        NextFrame { stream: self }
    }
}

type PollFrame = dyn FnMut(&mut Context<'_>) -> Poll<Option<Result<Frame>>> + Send;

struct PollFn {
    poll: Box<PollFrame>,
}

impl FrameSource for PollFn {
    fn poll_frame(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Result<Frame>>> {
        (self.get_mut().poll)(cx)
    }
}

struct Frames {
    frames: Box<dyn Iterator<Item = Result<Frame>> + Send>,
}

impl FrameSource for Frames {
    fn poll_frame(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Option<Result<Frame>>> {
        Poll::Ready(self.get_mut().frames.next())
    }
}

#[cfg(feature = "stream")]
struct FromStream {
    stream: Pin<Box<dyn futures_core::Stream<Item = Result<Frame>> + Send>>,
}

#[cfg(feature = "stream")]
impl FrameSource for FromStream {
    fn poll_frame(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Result<Frame>>> {
        self.get_mut().stream.as_mut().poll_next(cx)
    }
}

struct NextFrame<'a> {
    stream: &'a mut FrameStream,
}

impl Future for NextFrame<'_> {
    type Output = Option<Result<Frame>>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        self.get_mut().stream.poll_next(cx)
    }
}

impl std::fmt::Debug for FrameStream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("FrameStream")
    }
}

#[cfg(feature = "stream")]
#[cfg_attr(docsrs, doc(cfg(feature = "stream")))]
impl futures_core::Stream for FrameStream {
    type Item = Result<Frame>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.get_mut().poll_next(cx)
    }
}

/// Applies `f` to every frame of `stream`: `Ok(Some(frame))` passes a
/// (possibly rewritten) frame on, `Ok(None)` drops it, `Err` ends the run.
#[must_use]
pub fn map_frames(
    stream: FrameStream,
    f: impl FnMut(Frame) -> Result<Option<Frame>> + Send + 'static,
) -> FrameStream {
    FrameStream::new(MapFrames {
        stream,
        map: Box::new(f),
        done: false,
    })
}

type MapFrame = dyn FnMut(Frame) -> Result<Option<Frame>> + Send;

struct MapFrames {
    stream: FrameStream,
    map: Box<MapFrame>,
    done: bool,
}

impl FrameSource for MapFrames {
    fn poll_frame(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Result<Frame>>> {
        let this = self.get_mut();
        // This layer has its own per-call budget of dropped frames: a flood
        // of them yields to the executor instead of hogging it.
        let mut dropped = 0usize;
        loop {
            if this.done {
                return Poll::Ready(None);
            }
            match this.stream.poll_next(cx) {
                Poll::Ready(Some(Ok(frame))) => match (this.map)(frame) {
                    Ok(Some(frame)) => return Poll::Ready(Some(Ok(frame))),
                    Ok(None) => {
                        dropped += 1;
                        if dropped >= super::run::FRAMES_PER_POLL {
                            cx.waker().wake_by_ref();
                            return Poll::Pending;
                        }
                        continue;
                    }
                    Err(err) => {
                        this.done = true;
                        return Poll::Ready(Some(Err(err)));
                    }
                },
                Poll::Ready(Some(Err(err))) => {
                    this.done = true;
                    return Poll::Ready(Some(Err(err)));
                }
                Poll::Ready(None) => {
                    this.done = true;
                    return Poll::Ready(None);
                }
                Poll::Pending => return Poll::Pending,
            }
        }
    }
}

/// What a middleware receives besides the next stage.
#[derive(Clone)]
#[non_exhaustive]
pub struct MiddlewareRequest {
    /// The run input as it will be sent (after era downgrade, activity
    /// stripping and the typed-input check). Mutations are
    /// enforced again ([`Enforcer::enforce_outgoing_value`](crate::pipeline::enforce::Enforcer::enforce_outgoing_value))
    /// and then sent as made.
    pub input: Value,
    diagnostics: Arc<dyn DiagnosticSink>,
}

impl MiddlewareRequest {
    /// A request, for unit-testing a middleware.
    pub fn new(input: Value, diagnostics: Arc<dyn DiagnosticSink>) -> Self {
        MiddlewareRequest { input, diagnostics }
    }

    /// The run's diagnostic sink, for middleware that drops or rewrites
    /// material and must say so.
    #[must_use]
    pub fn diagnostics(&self) -> &Arc<dyn DiagnosticSink> {
        &self.diagnostics
    }
}

impl std::fmt::Debug for MiddlewareRequest {
    /// The input is not printed: `forwardedProps` and state may carry secrets.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MiddlewareRequest").finish_non_exhaustive()
    }
}

/// The rest of the chain. Call [`Next::run`] at most once.
pub struct Next {
    run: Box<dyn FnOnce(MiddlewareRequest) -> FrameStream + Send>,
}

impl Next {
    /// A next stage from a closure, for unit-testing a middleware.
    pub fn from_fn(f: impl FnOnce(MiddlewareRequest) -> FrameStream + Send + 'static) -> Self {
        Next { run: Box::new(f) }
    }

    /// Runs the rest of the chain with (possibly modified) input.
    #[must_use]
    pub fn run(self, request: MiddlewareRequest) -> FrameStream {
        (self.run)(request)
    }
}

impl std::fmt::Debug for Next {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Next")
    }
}

/// A middleware. Synchronous and object-safe: async work happens inside the
/// returned stream. Closures `Fn(MiddlewareRequest, Next) -> FrameStream`
/// implement it.
///
/// ```no_run
/// use agui_sdk::client::middleware::{FrameStream, Middleware, MiddlewareRequest, Next};
///
/// /// Adds a grounding entry to every run's input; passes frames through.
/// /// (`forwardedProps` is off limits: intermediaries never alter it, RUNIN-27.)
/// struct Locale;
///
/// impl Middleware for Locale {
///     fn run(&self, mut request: MiddlewareRequest, next: Next) -> FrameStream {
///         if let Some(context) = request.input.get_mut("context").and_then(|c| c.as_array_mut()) {
///             context.push(serde_json::json!({ "description": "locale", "value": "en-GB" }));
///         }
///         next.run(request)
///     }
/// }
/// ```
pub trait Middleware: Send + Sync + 'static {
    /// Handles one run.
    fn run(&self, request: MiddlewareRequest, next: Next) -> FrameStream;
}

impl<F> Middleware for F
where
    F: Fn(MiddlewareRequest, Next) -> FrameStream + Send + Sync + 'static,
{
    fn run(&self, request: MiddlewareRequest, next: Next) -> FrameStream {
        self(request, next)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use serde_json::json;

    fn frame(text: &'static str) -> Frame {
        Frame::from_json(Chunk::from(text))
    }

    #[test]
    fn the_type_scan_borrows_and_skips_flat_members() {
        let f = frame(
            r#" { "messageId" : "m1", "n": -1.5e3, "ok": true, "type": "TEXT_MESSAGE_END" }"#,
        );
        assert!(matches!(
            f.raw_type(),
            Some(Cow::Borrowed("TEXT_MESSAGE_END"))
        ));
        assert_eq!(f.event_type(), Some(EventType::TextMessageEnd));
        let f = frame(r#"{"type":"VENDOR_THING"}"#);
        assert_eq!(f.raw_type().as_deref(), Some("VENDOR_THING"));
        assert_eq!(f.event_type(), None);
    }

    #[test]
    fn the_type_scan_falls_back_to_parsing_when_inconclusive() {
        // After a nested object.
        let f = frame(r#"{"input":{"type":"no"},"type":"RUN_STARTED"}"#);
        assert_eq!(f.event_type(), Some(EventType::RunStarted));
        // An escape in the value or in a key.
        // `RUN_FINISHED` with its `F` written as a JSON unicode escape.
        let escaped = format!(r#"{{"type":"RUN_{}u0046INISHED"}}"#, '\\');
        let f = Frame::from_json(Chunk::from(escaped));
        assert!(matches!(f.raw_type(), Some(Cow::Owned(_))));
        assert_eq!(f.event_type(), Some(EventType::RunFinished));
        let f = frame(r#"{"a\"b":1,"type":"RAW"}"#);
        assert_eq!(f.event_type(), Some(EventType::Raw));
    }

    #[test]
    fn frames_without_a_string_type_have_none() {
        for text in [
            r#"{}"#,
            r#"{"a":1}"#,
            r#"{"type":3}"#,
            r#"[1]"#,
            "not json",
            r#"{"type":"#,
        ] {
            assert_eq!(frame(text).raw_type(), None, "{text}");
        }
        assert_eq!(Frame::from_value(json!({"type": 1})).raw_type(), None);
    }

    #[test]
    fn values_parse_once_and_reject_non_objects() {
        let mut f = frame(r#"{"type":"CUSTOM","name":"n","value":1}"#);
        f.value_mut().unwrap()["value"] = json!(2);
        assert_eq!(f.value().unwrap()["value"], 2);
        assert_eq!(f.raw_len(), None);
        assert_eq!(f.into_value().unwrap()["value"], 2);
        for text in ["[1]", "{", "\"s\""] {
            let err = frame(text).into_value().unwrap_err();
            assert_eq!(err.code(), ErrorCode::MalformedEvent, "{text}");
        }
    }

    #[test]
    fn map_frames_drops_rewrites_and_stops_at_an_error() {
        let frames = vec![
            Ok(Frame::from_value(json!({"type": "A"}))),
            Ok(Frame::from_value(json!({"type": "B"}))),
            Ok(Frame::from_value(json!({"type": "C"}))),
            Ok(Frame::from_value(json!({"type": "D"}))),
        ];
        let mut stream = map_frames(FrameStream::from_frames(frames), |frame| {
            match frame.raw_type().as_deref() {
                Some("A") => Ok(None),
                Some("C") => Err(Error::new(ErrorCode::MiddlewareFailed, "stop")),
                _ => Ok(Some(frame)),
            }
        });
        let waker = std::task::Waker::from(std::sync::Arc::new(NoopWake));
        let mut cx = Context::from_waker(&waker);
        let mut next = || match stream.poll_next(&mut cx) {
            Poll::Ready(item) => item,
            Poll::Pending => panic!("in-memory frames are always ready"),
        };
        assert_eq!(next().unwrap().unwrap().raw_type().as_deref(), Some("B"));
        assert_eq!(
            next().unwrap().unwrap_err().code(),
            ErrorCode::MiddlewareFailed
        );
        assert!(next().is_none());
        assert!(next().is_none());
    }

    struct NoopWake;

    impl std::task::Wake for NoopWake {
        fn wake(self: Arc<Self>) {}
    }
}
