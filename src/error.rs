//! The crate's one error type and its stable codes.
//!
//! Every fallible operation in this crate returns [`Error`]: client runs,
//! transports, the server runner, input admission, the protobuf codec and the
//! pipeline's fatal verdicts alike. Callers branch on [`Error::code`], a
//! [`ErrorCode`] whose string form (`"verify.tool_call"`) is stable across
//! releases and never reused, or on the coarser [`Error::kind`]. Messages are
//! for humans and may change; they never contain payload values or secrets.
//!
//! Non-fatal findings (a stripped field, a skipped patch) are not errors: they
//! are [`Diagnostic`](crate::diagnostics::Diagnostic)s.
//!
//! No third-party error type appears in this API; a transport's underlying
//! error is reachable only through [`std::error::Error::source`].
//!
//! [`Error`] is one pointer wide (its data is boxed): the error path is cold,
//! and every `Result` in the pipeline stays small.

use std::fmt;
use std::time::Duration;

/// The crate's result type.
pub type Result<T, E = Error> = std::result::Result<T, E>;

/// Any error, boxed. What [`server::Agent::run`][crate::server::Agent::run]
/// returns, so `?` works on application errors. The runner downcasts to
/// [`Error`] to honour its code and [`Error::public`] message; anything else
/// becomes `agent.failed` with a generic message (LIFE-21).
#[cfg_attr(
    not(feature = "server"),
    doc = "",
    doc = "[crate::server::Agent::run]: https://docs.rs/agui-sdk/latest/agui_sdk/server/trait.Agent.html#tymethod.run"
)]
pub type BoxError = Box<dyn std::error::Error + Send + Sync + 'static>;

/// Boxed source error.
type Source = BoxError;

macro_rules! error_codes {
    ($( $(#[$doc:meta])* $variant:ident = $text:literal, $kind:ident; )+) => {
        /// A stable error code. The string form is part of the public contract.
        ///
        /// `#[non_exhaustive]`: new codes are added in minor releases; match
        /// with a wildcard arm, or match on [`ErrorKind`].
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
        #[non_exhaustive]
        pub enum ErrorCode {
            $( $(#[$doc])* $variant, )+
        }

        impl ErrorCode {
            /// Every code this build knows.
            pub const ALL: &'static [ErrorCode] = &[ $( ErrorCode::$variant, )+ ];

            /// The stable dotted string, e.g. `"transport.timeout"`.
            #[must_use]
            pub const fn as_str(self) -> &'static str {
                match self { $( ErrorCode::$variant => $text, )+ }
            }

            /// The coarse class of the code.
            #[must_use]
            pub const fn kind(self) -> ErrorKind {
                match self { $( ErrorCode::$variant => ErrorKind::$kind, )+ }
            }

            /// Generated lookup behind [`ErrorCode::parse`]: a single `match`
            /// on the string, never a walk over [`ErrorCode::ALL`].
            fn from_known(text: &str) -> Option<Self> {
                match text {
                    $( $text => Some(ErrorCode::$variant), )+
                    _ => None,
                }
            }
        }
    };
}

error_codes! {
    /// Builder configuration is invalid (bad URL, reserved header, zero budget).
    InvalidConfig = "config.invalid", Usage;
    /// A request the caller built is invalid before anything is sent.
    InvalidRequest = "request.invalid", Usage;
    /// Connecting failed.
    Connect = "transport.connect", Transport;
    /// The connect, response-header or total timeout elapsed.
    Timeout = "transport.timeout", Transport;
    /// No bytes arrived within the idle timeout.
    IdleTimeout = "transport.idle_timeout", Transport;
    /// The server answered with a non-success HTTP status before any stream.
    HttpStatus = "transport.http_status", Transport;
    /// The response `Content-Type` is neither SSE nor (when enabled) protobuf.
    UnsupportedContentType = "transport.content_type", Transport;
    /// A transport read failed: the request before response headers arrived,
    /// or the response body; or a custom transport panicked while opening or
    /// reading ("the transport panicked").
    StreamRead = "transport.stream_read", Transport;
    /// SSE framing is invalid: a `data` line is not UTF-8 (comments and
    /// ignored fields such as `event`, `id` and `retry` may hold any bytes).
    /// An oversized line or event is [`ErrorCode::FrameTooLarge`].
    SseFraming = "framing.sse", Protocol;
    /// Protobuf framing or message bytes are invalid (PB-18).
    ProtoFraming = "framing.proto", Protocol;
    /// A frame exceeds the configured size budget.
    FrameTooLarge = "framing.too_large", Protocol;
    /// A frame is not a JSON object with a non-empty string `type`.
    MalformedEvent = "framing.malformed_event", Protocol;
    /// A known field carries a value the schema rejects (PROC-04, PROC-05).
    InvalidValue = "processing.invalid_value", Protocol;
    /// The peer declared a major version this build does not implement (VER-24).
    UnsupportedVersion = "processing.unsupported_version", Protocol;
    /// A container, depth or entry budget was exceeded.
    LimitExceeded = "processing.limit", Protocol;
    /// Run lifecycle violation (LIFE-03, LIFE-42, LIFE-53).
    RunLifecycle = "verify.run_lifecycle", Protocol;
    /// Text message streaming violation (TEXT-11).
    TextMessage = "verify.text_message", Protocol;
    /// Tool call streaming violation (TOOL-37).
    ToolCall = "verify.tool_call", Protocol;
    /// Step violation (LIFE-49..LIFE-51).
    Step = "verify.step", Protocol;
    /// Reasoning span or message violation (REAS-08..REAS-13).
    Reasoning = "verify.reasoning", Protocol;
    /// Subagent lifecycle violation (SUB-36).
    Subagent = "verify.subagent", Protocol;
    /// A continuation disagrees with its opener's owner (SUB-24).
    Ownership = "verify.ownership", Protocol;
    /// Chunk violation: missing first-chunk field, conflict, ambiguity (STRM-16, STRM-20, STRM-24).
    Chunk = "verify.chunk", Protocol;
    /// Activity event violation (ACT-19). Reserved: this release reports
    /// malformed activity events as `processing.invalid_value` and attribution
    /// mismatches as `verify.ownership`.
    Activity = "verify.activity", Protocol;
    /// Stream identity mismatch (run or thread ids disagree, RUNIN-04, RUNIN-05).
    Identity = "verify.identity", Protocol;
    /// The stream ended without a terminal event (TRN-09, SSE-15, PB-10).
    Truncated = "stream.truncated", Truncated;
    /// Resume entries do not exactly cover the open interrupts (INTR-10, INTR-11).
    ResumeCoverage = "resume.coverage", Conversation;
    /// A resolved entry answers an expired interrupt (INTR-14).
    InterruptExpired = "resume.expired", Conversation;
    /// Frontend tool calls are unanswered (TOOL-33).
    PendingToolCalls = "thread.pending_tool_calls", Conversation;
    /// Tool arguments failed validation against the tool's schema (TOOL-38).
    ToolArguments = "thread.tool_arguments", Conversation;
    /// The run was cancelled by the caller.
    Cancelled = "run.cancelled", Cancelled;
    /// An observer, subscriber or the diagnostic sink panicked; the run was
    /// failed and other observers finished.
    ObserverPanicked = "observer.panicked", Extension;
    /// A middleware failed or panicked.
    MiddlewareFailed = "middleware.failed", Extension;
    /// A capability declaration was rejected (CAP-15).
    CapabilitiesRejected = "capabilities.rejected", Protocol;
    /// The server agent returned an error.
    AgentFailed = "agent.failed", Server;
    /// The server agent panicked; the run ended with `RUN_ERROR`.
    AgentPanicked = "agent.panicked", Server;
    /// The producer verifier refused an event the agent emitted.
    ProducerViolation = "server.producer_violation", Server;
    /// The server's bounded event queue overflowed.
    QueueOverflow = "server.queue_overflow", Server;
    /// Run input was rejected before `RUN_STARTED` (RUNIN-29, PROC-35).
    InputRejected = "server.input_rejected", Server;
    /// No acceptable response media type (HTTP 406). The bundled HTTP
    /// handler never answers it: a producer answers SSE whenever it does not
    /// answer protobuf (PB-05).
    NotAcceptable = "server.not_acceptable", Server;
    /// Request media type unsupported (HTTP 415).
    UnsupportedMediaType = "server.unsupported_media_type", Server;
    /// Request body over the configured limit (HTTP 413).
    RequestTooLarge = "server.request_too_large", Server;
    /// Encoding an event for the wire failed.
    Encode = "server.encode", Server;
    /// The client disconnected.
    Disconnected = "server.disconnected", Server;
    /// SDK-side JSON (de)serialization failed (typed views such as `state_as`).
    Json = "internal.json", Internal;
    /// A bug in this crate. Please report it.
    Internal = "internal.bug", Internal;
}

impl ErrorCode {
    /// Parses the stable string form produced by [`ErrorCode::as_str`].
    ///
    /// Returns `None` for any string this build does not know, including
    /// codes added by newer releases. The lookup is a compiled `match` and
    /// allocates nothing.
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        Self::from_known(text)
    }

    /// Whether the code reports a protocol violation by the peer, i.e. its
    /// [`kind`](ErrorCode::kind) is [`ErrorKind::Protocol`].
    #[must_use]
    pub const fn is_protocol_violation(self) -> bool {
        matches!(self.kind(), ErrorKind::Protocol)
    }
}

impl fmt::Display for ErrorCode {
    /// Writes the stable dotted string.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Coarse error classes, for callers that do not need individual codes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ErrorKind {
    /// The caller misused the API.
    Usage,
    /// Network or HTTP failure before or while reading the stream.
    Transport,
    /// The peer violated the protocol; the run fails (it is never "repaired").
    Protocol,
    /// The stream ended without a terminal event. Not a success (TRN-10).
    Truncated,
    /// A conversation rule (resume coverage, pending calls) blocked a send.
    Conversation,
    /// Cancelled by the caller.
    Cancelled,
    /// An extension (observer, middleware) failed.
    Extension,
    /// Server-side failure.
    Server,
    /// A bug in this crate.
    Internal,
}

impl ErrorKind {
    /// The class as a stable lowercase word: `"usage"`, `"transport"`,
    /// `"protocol"` and so on.
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        match self {
            ErrorKind::Usage => "usage",
            ErrorKind::Transport => "transport",
            ErrorKind::Protocol => "protocol",
            ErrorKind::Truncated => "truncated",
            ErrorKind::Conversation => "conversation",
            ErrorKind::Cancelled => "cancelled",
            ErrorKind::Extension => "extension",
            ErrorKind::Server => "server",
            ErrorKind::Internal => "internal",
        }
    }
}

impl fmt::Display for ErrorKind {
    /// Writes [`as_str`](Self::as_str).
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The crate's error.
///
/// Invariants: `kind() == code().kind()`; `is_retryable()` implies
/// `!run_started()`; when the run had started, a client error carries the
/// partial report of everything delivered before the failure (LIFE-19,
/// TRN-11). One exception: when the history limit
/// ([`max_history_bytes`](crate::pipeline::Limits::max_history_bytes)) ends
/// the run, the partial history also holds the effect of the one event that
/// crossed the limit, which was not delivered.
pub struct Error {
    inner: Box<ErrorInner>,
}

struct ErrorInner {
    code: ErrorCode,
    message: Box<str>,
    pointer: Option<Box<str>>,
    source: Option<Source>,
    http_status: Option<u16>,
    retry_after: Option<Duration>,
    run_started: bool,
    public: bool,
    #[cfg(feature = "client")]
    partial: Option<crate::client::RunReport>,
}

impl Error {
    /// An error with a code and a human-readable message.
    ///
    /// The message must not contain payload values or secrets; it is meant
    /// for logs. Attach detail with the `with_*` methods.
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Error {
            inner: Box::new(ErrorInner {
                code,
                message: message.into().into_boxed_str(),
                pointer: None,
                source: None,
                http_status: None,
                retry_after: None,
                run_started: false,
                public: false,
                #[cfg(feature = "client")]
                partial: None,
            }),
        }
    }

    /// Attaches the underlying cause, reachable through
    /// [`std::error::Error::source`].
    #[must_use]
    pub fn with_source(mut self, source: impl Into<Source>) -> Self {
        self.inner.source = Some(source.into());
        self
    }

    /// Attaches the RFC 6901 JSON pointer of the offending value.
    #[must_use]
    pub fn with_pointer(mut self, pointer: impl Into<String>) -> Self {
        self.inner.pointer = Some(pointer.into().into_boxed_str());
        self
    }

    /// Attaches the HTTP status of a rejection received before any stream.
    #[must_use]
    pub fn with_http_status(mut self, status: u16) -> Self {
        self.inner.http_status = Some(status);
        self
    }

    /// Attaches the delay the server asked for (`Retry-After`).
    #[must_use]
    pub fn with_retry_after(mut self, delay: Duration) -> Self {
        self.inner.retry_after = Some(delay);
        self
    }

    /// Marks the error as having happened after `RUN_STARTED` was delivered.
    /// Such an error is never retryable.
    #[allow(dead_code)] // used by the client and server runtimes
    pub(crate) fn with_run_started(mut self) -> Self {
        self.inner.run_started = true;
        self
    }

    /// Attaches the report of everything delivered before the failure, and
    /// marks the run as started.
    #[cfg(feature = "client")]
    #[allow(dead_code)] // used by the client runtime
    pub(crate) fn with_partial(mut self, report: crate::client::RunReport) -> Self {
        self.inner.run_started = true;
        self.inner.partial = Some(report);
        self
    }

    /// Marks the message as safe to show to the client. A server agent's
    /// error marked public reaches `RUN_ERROR.message` (with its code)
    /// verbatim; an unmarked one is replaced by a generic message (LIFE-21).
    #[must_use]
    pub fn public(mut self) -> Self {
        self.inner.public = true;
        self
    }

    /// Whether [`public`](Self::public) was set.
    #[must_use]
    pub fn is_public(&self) -> bool {
        self.inner.public
    }

    /// The stable code.
    #[must_use]
    pub fn code(&self) -> ErrorCode {
        self.inner.code
    }

    /// The coarse class; always `self.code().kind()`.
    #[must_use]
    pub fn kind(&self) -> ErrorKind {
        self.inner.code.kind()
    }

    /// The human-readable message. Contains no payload values.
    #[must_use]
    pub fn message(&self) -> &str {
        &self.inner.message
    }

    /// JSON pointer of the offending value, for validation errors.
    #[must_use]
    pub fn pointer(&self) -> Option<&str> {
        self.inner.pointer.as_deref()
    }

    /// HTTP status, for rejections received before any stream.
    #[must_use]
    pub fn http_status(&self) -> Option<u16> {
        self.inner.http_status
    }

    /// Server-requested retry delay.
    #[must_use]
    pub fn retry_after(&self) -> Option<Duration> {
        self.inner.retry_after
    }

    /// Whether retrying might succeed: connect errors and 429/502/503/504
    /// received before the stream started. Never true once a run started. A
    /// 502/504 from a gateway does not prove the agent never received the
    /// request, so retrying a non-idempotent run can duplicate it.
    #[must_use]
    pub fn is_retryable(&self) -> bool {
        if self.inner.run_started {
            return false;
        }
        match self.inner.code {
            ErrorCode::Connect => true,
            ErrorCode::HttpStatus => {
                matches!(self.inner.http_status, Some(429 | 502 | 503 | 504))
            }
            _ => false,
        }
    }

    /// Whether the peer violated the protocol.
    #[must_use]
    pub fn is_protocol_violation(&self) -> bool {
        self.inner.code.is_protocol_violation()
    }

    /// Whether `RUN_STARTED` had been delivered when the error occurred.
    #[must_use]
    pub fn run_started(&self) -> bool {
        self.inner.run_started
    }

    /// Everything delivered before the failure, when the run had started.
    #[cfg(feature = "client")]
    #[cfg_attr(docsrs, doc(cfg(feature = "client")))]
    #[must_use]
    pub fn partial(&self) -> Option<&crate::client::RunReport> {
        self.inner.partial.as_ref()
    }

    /// Takes the partial report.
    #[cfg(feature = "client")]
    #[cfg_attr(docsrs, doc(cfg(feature = "client")))]
    #[must_use]
    pub fn into_partial(self) -> Option<crate::client::RunReport> {
        self.inner.partial
    }
}

impl fmt::Debug for Error {
    /// Shows the code, message and attached metadata. The partial report is
    /// summarised as present or absent; its payloads are never printed.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let inner = &*self.inner;
        let mut s = f.debug_struct("Error");
        s.field("code", &inner.code.as_str())
            .field("message", &&*inner.message);
        if let Some(pointer) = &inner.pointer {
            s.field("pointer", &&**pointer);
        }
        if let Some(status) = inner.http_status {
            s.field("http_status", &status);
        }
        if let Some(delay) = inner.retry_after {
            s.field("retry_after", &delay);
        }
        s.field("run_started", &inner.run_started)
            .field("public", &inner.public);
        #[cfg(feature = "client")]
        s.field("partial", &inner.partial.is_some());
        if let Some(source) = &inner.source {
            s.field("source", source);
        }
        s.finish()
    }
}

impl fmt::Display for Error {
    /// `"<code>: <message>"`, with `" (at <pointer>)"` appended when present.
    /// Control characters are escaped and the pointer is cut to a bounded
    /// preview; [`Error::message`] and [`Error::pointer`] stay exact.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        use crate::diagnostics::{bounded_pointer, write_escaped};
        f.write_str(self.inner.code.as_str())?;
        f.write_str(": ")?;
        // Peer-chosen text: escaped so it cannot forge log lines.
        write_escaped(f, &self.inner.message)?;
        if let Some(pointer) = &self.inner.pointer {
            f.write_str(" (at ")?;
            write_escaped(f, &bounded_pointer(pointer))?;
            f.write_str(")")?;
        }
        Ok(())
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match &self.inner.source {
            Some(source) => Some(&**source),
            None => None,
        }
    }
}

// The error path is cold: keep every `Result<T>` in the pipeline small.
const _: () = assert!(std::mem::size_of::<Error>() == std::mem::size_of::<usize>());
const _: () = assert!(std::mem::size_of::<Result<()>>() == std::mem::size_of::<usize>());

#[cfg(test)]
mod tests {
    use super::*;

    const fn assert_send_sync<T: Send + Sync + 'static>() {}
    const _: () = {
        assert_send_sync::<Error>();
        assert_send_sync::<BoxError>();
        assert_send_sync::<ErrorCode>();
        assert_send_sync::<ErrorKind>();
    };

    #[test]
    fn codes_round_trip_through_parse_and_are_unique() {
        let mut seen = std::collections::BTreeSet::new();
        for &code in ErrorCode::ALL {
            assert!(seen.insert(code.as_str()), "duplicate {}", code);
            assert_eq!(ErrorCode::parse(code.as_str()), Some(code));
            assert_eq!(code.to_string(), code.as_str());
            let (family, name) = code.as_str().split_once('.').unwrap_or(("", ""));
            assert!(
                !family.is_empty() && !name.is_empty(),
                "{code} is not dotted"
            );
        }
        assert_eq!(
            ErrorCode::parse("verify.tool_call"),
            Some(ErrorCode::ToolCall)
        );
        assert_eq!(ErrorCode::parse("Verify.tool_call"), None);
        assert_eq!(ErrorCode::parse(""), None);
        assert_eq!(ErrorCode::parse("verify.tool_call "), None);
    }

    #[test]
    fn protocol_violation_follows_kind() {
        for &code in ErrorCode::ALL {
            assert_eq!(
                code.is_protocol_violation(),
                code.kind() == ErrorKind::Protocol
            );
            let error = Error::new(code, "x");
            assert_eq!(error.kind(), code.kind());
            assert_eq!(error.is_protocol_violation(), code.is_protocol_violation());
        }
        assert!(ErrorCode::ToolCall.is_protocol_violation());
        assert!(!ErrorCode::Truncated.is_protocol_violation());
        assert!(!ErrorCode::Connect.is_protocol_violation());
    }

    #[test]
    fn accessors_return_what_was_attached() {
        let error = Error::new(ErrorCode::InvalidValue, "bad role")
            .with_pointer("/messages/0/role")
            .with_http_status(400)
            .with_retry_after(Duration::from_secs(3));
        assert_eq!(error.code(), ErrorCode::InvalidValue);
        assert_eq!(error.message(), "bad role");
        assert_eq!(error.pointer(), Some("/messages/0/role"));
        assert_eq!(error.http_status(), Some(400));
        assert_eq!(error.retry_after(), Some(Duration::from_secs(3)));
        assert!(!error.run_started());
        let bare = Error::new(ErrorCode::Internal, "bug");
        assert_eq!(bare.pointer(), None);
        assert_eq!(bare.http_status(), None);
        assert_eq!(bare.retry_after(), None);
    }

    #[test]
    fn display_is_code_message_and_pointer() {
        let error = Error::new(ErrorCode::InvalidValue, "bad role");
        assert_eq!(error.to_string(), "processing.invalid_value: bad role");
        let error = error.with_pointer("/role");
        assert_eq!(
            error.to_string(),
            "processing.invalid_value: bad role (at /role)"
        );
        let debug = format!("{error:?}");
        assert!(debug.contains("processing.invalid_value") && debug.contains("/role"));
    }

    #[test]
    fn display_escapes_and_bounds_peer_text() {
        let pointer = format!("/a\u{1b}[2J/{}", "k".repeat(10_000));
        let error =
            Error::new(ErrorCode::InvalidValue, "bad\nWARN forged").with_pointer(pointer.clone());
        let shown = error.to_string();
        assert!(
            !shown.contains('\n') && !shown.contains('\u{1b}'),
            "{shown}"
        );
        assert!(
            shown.contains("\\n") && shown.contains("\\u{1b}"),
            "{shown}"
        );
        assert!(shown.len() < 600, "display is {} bytes", shown.len());
        assert!(shown.contains('…'), "{shown}");
        assert_eq!(error.pointer(), Some(pointer.as_str()));
    }

    #[test]
    fn retryable_only_before_the_run_for_connect_and_transient_statuses() {
        assert!(Error::new(ErrorCode::Connect, "refused").is_retryable());
        for status in [429, 502, 503, 504] {
            let error = Error::new(ErrorCode::HttpStatus, "busy").with_http_status(status);
            assert!(error.is_retryable(), "{status}");
        }
        for status in [400, 401, 404, 500, 501] {
            let error = Error::new(ErrorCode::HttpStatus, "no").with_http_status(status);
            assert!(!error.is_retryable(), "{status}");
        }
        assert!(!Error::new(ErrorCode::HttpStatus, "no status").is_retryable());
        assert!(!Error::new(ErrorCode::Timeout, "slow").is_retryable());
        assert!(!Error::new(ErrorCode::ToolCall, "bad").is_retryable());
        let started = Error::new(ErrorCode::Connect, "reset").with_run_started();
        assert!(started.run_started());
        assert!(!started.is_retryable());
    }

    #[test]
    fn public_marks_the_message_shareable() {
        let error = Error::new(ErrorCode::AgentFailed, "quota exhausted");
        assert!(!error.is_public());
        let error = error.public();
        assert!(error.is_public());
        assert_eq!(error.message(), "quota exhausted");
        assert_eq!(error.code(), ErrorCode::AgentFailed);
    }

    #[test]
    fn error_is_one_pointer_wide() {
        assert_eq!(std::mem::size_of::<Error>(), std::mem::size_of::<usize>());
    }

    #[test]
    fn source_is_reachable() {
        let io = std::io::Error::other("socket closed");
        let error = Error::new(ErrorCode::StreamRead, "read failed").with_source(io);
        let source = std::error::Error::source(&error).map(|s| s.to_string());
        assert_eq!(source.as_deref(), Some("socket closed"));
        assert!(std::error::Error::source(&Error::new(ErrorCode::Internal, "x")).is_none());
    }
}
