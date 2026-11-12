//! Non-fatal findings: what was stripped, dropped, translated or skipped.
//!
//! The spec asks for warnings in many places (PROC-12, PROC-13, PROC-33,
//! VER-07, SNAP-12, ACT-13, REAS-17, REAS-20, INTR-19, ...) and forbids
//! silencing lossy-translation warnings by default (VER-11). This module is the
//! single channel for all of them:
//!
//! * a [`Diagnostic`] has a stable [`DiagnosticCode`], a JSON pointer, the event
//!   type it concerns, and a short message free of payload values;
//! * a [`DiagnosticSink`] receives them. [`default_sink`] routes to `tracing`
//!   (feature `tracing`) when a subscriber is interested, else to `log`
//!   (feature `log`) when a logger is installed, else to standard error. Only
//!   an explicit [`NullSink`] is silent;
//! * a [`Reporter`] caps how many one event may produce
//!   ([`DEFAULT_MAX_PER_EVENT`]) and emits one `diagnostics.limit` summary for
//!   the rest, so a hostile stream cannot flood logs.
//!
//! A conformant stream draws no diagnostics at all (PROC-37). The hot path pays
//! nothing for this module when there is nothing to report: no allocation, no
//! virtual call.
//!
//! # Routing
//!
//! [`DefaultSink`] decides per diagnostic, with constant-time probes:
//!
//! 1. feature `tracing`: if a `tracing` subscriber is interested in
//!    [`TARGET`] at `WARN` (the interest is cached per call site by
//!    `tracing` itself), the diagnostic becomes a `WARN` event with the
//!    fields `code`, `scope`, `pointer`, `event_type` and `run_id`;
//! 2. feature `log`: otherwise, if the installed `log` logger accepts
//!    [`TARGET`] at `Warn`, it becomes a `log` record;
//! 3. otherwise it is written to standard error as one line.
//!
//! The decision is re-made for each diagnostic rather than frozen at the first
//! one, so a subscriber or logger installed after start-up takes over, and a
//! diagnostic is never dropped because a backend appeared or changed its
//! filter later.

use crate::event::EventType;
use crate::ids::RunId;
use std::fmt;
use std::io::Write;
use std::sync::{Arc, Mutex, OnceLock, PoisonError};

/// `tracing` target and `log` target used by [`DefaultSink`].
pub const TARGET: &str = "agui_sdk::diagnostics";

/// Default cap on diagnostics per event, after which one summary is reported.
pub const DEFAULT_MAX_PER_EVENT: usize = 256;

macro_rules! diagnostic_codes {
    ($( $(#[$doc:meta])* $variant:ident = $text:literal; )+) => {
        /// Stable diagnostic code; the string form is part of the public contract.
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
        #[non_exhaustive]
        pub enum DiagnosticCode {
            $( $(#[$doc])* $variant, )+
        }

        impl DiagnosticCode {
            /// Every code this build knows.
            pub const ALL: &'static [DiagnosticCode] = &[ $( DiagnosticCode::$variant, )+ ];

            /// The stable dotted string.
            #[must_use]
            pub const fn as_str(self) -> &'static str {
                match self { $( DiagnosticCode::$variant => $text, )+ }
            }
        }
    };
}

diagnostic_codes! {
    /// An event of an unknown type was dropped (PROC-01, PROC-12).
    UnknownEvent = "drop.unknown_event";
    /// A frame whose protobuf envelope arm is unknown was dropped (PROC-08, PB-17).
    UnknownProtobufArm = "drop.unknown_protobuf_arm";
    /// An undescribed property was stripped (PROC-02, PROC-13).
    UnknownField = "strip.unknown_field";
    /// An unrecognised union member (content part, outcome) was stripped (PROC-03, PROC-18..PROC-20).
    UnknownVariant = "strip.unknown_variant";
    /// An unrecognised value of a closed string set was stripped (PROC-17, REAS-23).
    UnknownEnumValue = "strip.unknown_enum_value";
    /// A JSON Patch operation with an unknown `op` was dropped (SNAP-10).
    UnknownPatchOp = "strip.unknown_patch_op";
    /// A `null` in an optional position was read as absent (compatibility shim, BASIC-12).
    OptionalNull = "compat.optional_null";
    /// A retired event shape was translated to its replacement (VER-13).
    RetiredShape = "compat.retired_shape";
    /// Legacy binary content was upgraded to media parts (VER-13).
    LegacyContent = "compat.legacy_content";
    /// A downgrade for an older peer lost information (VER-07..VER-09, TOOL-22).
    LossyDowngrade = "downgrade.lossy";
    /// The peer declared a newer minor version (VER-23, VER-26).
    NewerMinorVersion = "version.newer_minor";
    /// The peer's declared version could not be interpreted (VER-21).
    UninterpretableVersion = "version.uninterpretable";
    /// The peer declared a major line this build does not implement (VER-24).
    UnsupportedMajorVersion = "version.unsupported_major";
    /// A well-formed patch did not apply; the prior value was kept (SNAP-12..SNAP-14).
    PatchNotApplied = "patch.not_applied";
    /// A patch would exceed the node budget; the prior value was kept.
    PatchBudgetExceeded = "patch.budget_exceeded";
    /// A `MESSAGES_SNAPSHOT` would have moved more held messages than the
    /// work bound allows; the snapshot was not applied.
    SnapshotWorkBound = "snapshot.work_bound";
    /// An activity delta named no activity message and was skipped (ACT-13).
    MissingActivityTarget = "activity.missing_target";
    /// A content, argument or end event named a message or call the reducer
    /// no longer holds (a `MESSAGES_SNAPSHOT` may have dropped it); the
    /// fragment was discarded.
    MissingFragmentTarget = "stream.missing_target";
    /// A reopen of a closed message or call disagreed with it (TEXT-07, TOOL-08).
    ConflictingReopen = "stream.conflicting_reopen";
    /// An encrypted value named nothing known and was dropped (REAS-20, REAS-21).
    UnknownEncryptedEntity = "reasoning.unknown_entity";
    /// An encrypted value could not be preserved (REAS-17).
    /// Reserved: this release does not emit it. A value naming an activity
    /// message is the unknown-`entityId` case (REAS-16) and reports
    /// `reasoning.unknown_entity`.
    EncryptedValueLost = "reasoning.value_lost";
    /// A resume entry names an interrupt the producer did not raise (INTR-18, INTR-19).
    UnknownResumeEntry = "resume.unknown_entry";
    /// An interrupt's `expiresAt` could not be parsed; it never expires.
    UnparseableExpiry = "interrupt.unparseable_expiry";
    /// A content part the producer cannot use was skipped (RUNIN-11, RUNIN-16, TOOL-17).
    ContentPartSkipped = "content.part_skipped";
    /// Content was flattened to text for a string-only reader (TOOL-20).
    /// Reserved: this release reports lossy flattening as `downgrade.lossy`
    /// (emitted by the 0.0.39 downgrade shim); lossless flattening is silent.
    ContentFlattened = "content.flattened";
    /// Metadata tried to write the reserved `ag-ui` key (META-04).
    ReservedMetadataKey = "metadata.reserved_key";
    /// A capability declaration was rejected and ignored (CAP-15, CAP-16).
    CapabilitiesIgnored = "capabilities.ignored";
    /// More diagnostics were suppressed by a cap. Per event or per capability
    /// declaration, the extra diagnostics were dropped. As the last entry of
    /// `RunReport::diagnostics` (scope `Scope::Stream`), more than
    /// `Limits::max_report_diagnostics` were not kept in the report; the
    /// diagnostic sink and observers still saw every one.
    Limit = "diagnostics.limit";
}

impl fmt::Display for DiagnosticCode {
    /// Writes the stable dotted string.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Which document a diagnostic's pointer is rooted in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Scope {
    /// A received event.
    Event,
    /// Run input (outgoing on the client, received on the server).
    RunInput,
    /// A capability declaration.
    Capabilities,
    /// Neither: a stream-level or configuration finding.
    Stream,
}

impl Scope {
    /// Lower-case name used in rendered diagnostics.
    const fn label(self) -> &'static str {
        match self {
            Scope::Event => "event",
            Scope::RunInput => "run input",
            Scope::Capabilities => "capabilities",
            Scope::Stream => "stream",
        }
    }
}

/// Upper bound on [`Diagnostic::message`], in bytes.
const MAX_MESSAGE_BYTES: usize = 192;

/// Upper bound on [`Diagnostic::pointer`], in bytes, before the `…` marker.
const MAX_POINTER_BYTES: usize = 256;

/// Upper bound on one pointer token (a peer-chosen member name, usually), in
/// bytes, before the `…` marker.
const MAX_TOKEN_BYTES: usize = 64;

/// Upper bound on [`Diagnostic::unknown_type`], in bytes, before the `…`
/// marker.
pub(crate) const MAX_UNKNOWN_TYPE_BYTES: usize = 64;

/// Marks a peer string cut short.
const CUT: &str = "\u{2026}";

/// At most `max` bytes of `text` (cut at a character boundary), followed by
/// `…` when anything was cut: a bounded preview of a peer-chosen string.
pub(crate) fn preview(text: &str, max: usize) -> std::borrow::Cow<'_, str> {
    if text.len() <= max {
        return std::borrow::Cow::Borrowed(text);
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    std::borrow::Cow::Owned(format!("{}{CUT}", &text[..end]))
}

/// Longest peer-chosen id quoted in error text, in bytes.
pub(crate) const MAX_ID_PREVIEW: usize = 64;

/// A peer-chosen id, cut to a bounded preview for error text.
pub(crate) fn shown(id: &str) -> String {
    // Escaped so control characters (newlines, ANSI) cannot forge log lines.
    preview(id, MAX_ID_PREVIEW).escape_debug().collect()
}

/// `pointer` with every token over [`MAX_TOKEN_BYTES`] and the whole over
/// [`MAX_POINTER_BYTES`] cut to a preview. Allocates only when cutting.
pub(crate) fn bounded_pointer(pointer: &str) -> std::borrow::Cow<'_, str> {
    let long_token = pointer.split('/').any(|t| t.len() > MAX_TOKEN_BYTES);
    if pointer.len() <= MAX_POINTER_BYTES && !long_token {
        return std::borrow::Cow::Borrowed(pointer);
    }
    let mut out = String::with_capacity(MAX_POINTER_BYTES + CUT.len());
    for (i, token) in pointer.split('/').enumerate() {
        if i > 0 {
            out.push('/');
        }
        out.push_str(&preview(token, MAX_TOKEN_BYTES));
        if out.len() > MAX_POINTER_BYTES {
            break;
        }
    }
    match preview(&out, MAX_POINTER_BYTES) {
        std::borrow::Cow::Owned(cut) => std::borrow::Cow::Owned(cut),
        std::borrow::Cow::Borrowed(_) => std::borrow::Cow::Owned(out),
    }
}

/// [`bounded_pointer`] for an owned pointer; allocates only when cutting.
fn bound_pointer(pointer: String) -> String {
    match bounded_pointer(&pointer) {
        std::borrow::Cow::Borrowed(_) => pointer,
        std::borrow::Cow::Owned(cut) => cut,
    }
}

/// One non-fatal finding.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Diagnostic {
    /// Stable code.
    pub code: DiagnosticCode,
    /// The document the pointer is rooted in.
    pub scope: Scope,
    /// RFC 6901 pointer to the affected value (`""` for the whole document).
    /// Peer-chosen, so bounded: a token over 64 bytes, and the pointer past
    /// 256 bytes, are cut short and marked with `…`.
    pub pointer: String,
    /// Type of the event concerned, when it is a known type.
    pub event_type: Option<EventType>,
    /// Wire `type` of an unknown event (`drop.unknown_event` only), cut
    /// short past 64 bytes and marked with `…`.
    pub unknown_type: Option<Box<str>>,
    /// Run the finding belongs to, when known, for audit correlation (IDX-11).
    pub run_id: Option<RunId>,
    /// Short human-readable explanation; no payload values, at most 192 bytes.
    /// For lossy downgrades it names what was lost and what to upgrade (VER-08, VER-09).
    pub message: String,
}

impl Diagnostic {
    /// A diagnostic for `scope` at `pointer`.
    ///
    /// A message longer than 192 bytes is cut at the last character boundary
    /// that fits, in place. A long pointer is cut to a bounded preview (see
    /// [`pointer`](Self::pointer)).
    pub fn new(
        code: DiagnosticCode,
        scope: Scope,
        pointer: impl Into<String>,
        message: impl Into<String>,
    ) -> Self {
        let mut message = message.into();
        truncate_at_boundary(&mut message, MAX_MESSAGE_BYTES);
        Diagnostic {
            code,
            scope,
            pointer: bound_pointer(pointer.into()),
            event_type: None,
            unknown_type: None,
            run_id: None,
            message,
        }
    }

    /// Sets the event type concerned.
    #[must_use]
    pub fn with_event_type(mut self, event_type: EventType) -> Self {
        self.event_type = Some(event_type);
        self
    }

    /// Sets the wire `type` of an unknown event, cut to a 64-byte preview.
    #[must_use]
    pub fn with_unknown_type(mut self, wire_type: impl Into<Box<str>>) -> Self {
        let wire_type = wire_type.into();
        self.unknown_type = Some(match preview(&wire_type, MAX_UNKNOWN_TYPE_BYTES) {
            std::borrow::Cow::Borrowed(_) => wire_type,
            std::borrow::Cow::Owned(cut) => cut.into_boxed_str(),
        });
        self
    }

    /// Sets the run the finding belongs to.
    #[must_use]
    pub fn with_run_id(mut self, run_id: RunId) -> Self {
        self.run_id = Some(run_id);
        self
    }

    /// The wire `type` concerned, known or not.
    fn wire_type(&self) -> Option<&str> {
        match (&self.event_type, &self.unknown_type) {
            (Some(known), _) => Some(known.as_str()),
            (None, Some(unknown)) => Some(unknown),
            (None, None) => None,
        }
    }
}

/// Shortens `text` to at most `max` bytes without splitting a character.
fn truncate_at_boundary(text: &mut String, max: usize) {
    if text.len() <= max {
        return;
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text.truncate(end);
}

/// Whether [`write_escaped`] escapes `c`: control characters, the Unicode
/// line and paragraph separators, and the bidi marks, embeddings, overrides
/// and isolates (all of which `escape_debug` in [`shown`] escapes too).
fn needs_escape(c: char) -> bool {
    c.is_control()
        || matches!(
            c,
            '\u{200E}' | '\u{200F}' | '\u{2028}'..='\u{202E}' | '\u{2066}'..='\u{2069}'
        )
}

/// Writes `text`, escaping control characters, line separators and bidi
/// controls so that peer-chosen strings (an unknown event type, a pointer)
/// cannot forge extra log lines or reorder what is shown.
pub(crate) fn write_escaped(f: &mut fmt::Formatter<'_>, text: &str) -> fmt::Result {
    let mut rest = text;
    while let Some(at) = rest.find(needs_escape) {
        f.write_str(&rest[..at])?;
        let mut chars = rest[at..].chars();
        if let Some(c) = chars.next() {
            write!(f, "{}", c.escape_default())?;
        }
        rest = chars.as_str();
    }
    f.write_str(rest)
}

impl fmt::Display for Diagnostic {
    /// `"<code>: <message> (<scope>[ <event type>][ run <run id>][ at
    /// <pointer>])"`, for example `strip.unknown_field: undescribed property
    /// stripped (event TEXT_MESSAGE_START run r-1 at /extra)`. The event type
    /// is the known type or, failing that, the unknown wire type. Control
    /// characters are escaped.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.code.as_str())?;
        f.write_str(": ")?;
        write_escaped(f, &self.message)?;
        f.write_str(" (")?;
        f.write_str(self.scope.label())?;
        if let Some(wire_type) = self.wire_type() {
            f.write_str(" ")?;
            write_escaped(f, wire_type)?;
        }
        if let Some(run_id) = &self.run_id {
            f.write_str(" run ")?;
            write_escaped(f, run_id.as_str())?;
        }
        if !self.pointer.is_empty() {
            f.write_str(" at ")?;
            write_escaped(f, &self.pointer)?;
        }
        f.write_str(")")
    }
}

/// Receives diagnostics. Implementations must not panic and must not block;
/// they are called synchronously from the pipeline. The client and the
/// server runner contain a sink that panics anyway: the run fails (the client
/// with `observer.panicked`, the server with a `RUN_ERROR` of code
/// `agent.panicked`) and still ends with exactly one terminal.
///
/// Implemented for closures `Fn(&Diagnostic) + Send + Sync` and for `Arc<S>`.
pub trait DiagnosticSink: Send + Sync {
    /// Handles one diagnostic.
    fn report(&self, diagnostic: &Diagnostic);

    /// Whether this sink handles diagnostics at all. `false` (as for
    /// [`NullSink`]) lets a [`Reporter`] skip building them: the pipeline
    /// then spends no allocation on a finding nobody receives. Asked once,
    /// when the reporter is created. Defaults to `true`.
    fn enabled(&self) -> bool {
        true
    }
}

impl<F> DiagnosticSink for F
where
    F: Fn(&Diagnostic) + Send + Sync,
{
    fn report(&self, diagnostic: &Diagnostic) {
        self(diagnostic);
    }
}

impl<S: DiagnosticSink + ?Sized> DiagnosticSink for Arc<S> {
    fn report(&self, diagnostic: &Diagnostic) {
        (**self).report(diagnostic);
    }

    fn enabled(&self) -> bool {
        (**self).enabled()
    }
}

/// A user sink behind `catch_unwind`: a panic is recorded, the sink is not
/// called again, and the server runner fails the run instead of unwinding
/// through its poll.
#[cfg(feature = "server")]
pub(crate) struct GuardedSink {
    inner: Arc<dyn DiagnosticSink>,
    panicked: std::sync::atomic::AtomicBool,
}

#[cfg(feature = "server")]
impl GuardedSink {
    pub(crate) fn new(inner: Arc<dyn DiagnosticSink>) -> Self {
        GuardedSink {
            inner,
            panicked: std::sync::atomic::AtomicBool::new(false),
        }
    }

    /// Whether the wrapped sink has panicked.
    pub(crate) fn panicked(&self) -> bool {
        self.panicked.load(std::sync::atomic::Ordering::Relaxed)
    }
}

#[cfg(feature = "server")]
impl DiagnosticSink for GuardedSink {
    fn report(&self, diagnostic: &Diagnostic) {
        if self.panicked() {
            return;
        }
        let inner = &self.inner;
        let call = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            inner.report(diagnostic);
        }));
        if call.is_err() {
            self.panicked
                .store(true, std::sync::atomic::Ordering::Relaxed);
        }
    }

    fn enabled(&self) -> bool {
        self.inner.enabled()
    }
}

/// Where [`DefaultSink`] sends one diagnostic.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Route {
    /// A `tracing` subscriber wants warnings on [`TARGET`].
    #[cfg(feature = "tracing")]
    Tracing,
    /// The `log` logger accepts warnings on [`TARGET`].
    #[cfg(feature = "log")]
    Log,
    /// Neither is available: standard error.
    Stderr,
}

/// Picks the backend for the next diagnostic. Each probe is a constant-time
/// check against state the backend already caches (call-site interest for
/// `tracing`, the max level and logger filter for `log`).
fn route() -> Route {
    #[cfg(feature = "tracing")]
    {
        if tracing::enabled!(target: TARGET, tracing::Level::WARN) {
            return Route::Tracing;
        }
    }
    #[cfg(feature = "log")]
    {
        if log::log_enabled!(target: TARGET, log::Level::Warn) {
            return Route::Log;
        }
    }
    Route::Stderr
}

/// Writes the standard-error form of one diagnostic as a single line.
fn write_line(out: &mut impl Write, diagnostic: &Diagnostic) -> std::io::Result<()> {
    // Standard error is unbuffered: format first so the line is one write.
    let line = format!("WARN {TARGET}: {diagnostic}\n");
    out.write_all(line.as_bytes())
}

/// The never-silent default: `tracing` → `log` → standard error, at warning
/// level, target [`TARGET`]. See the module docs for the routing rule.
#[derive(Debug, Clone, Copy, Default)]
pub struct DefaultSink;

impl DiagnosticSink for DefaultSink {
    fn report(&self, diagnostic: &Diagnostic) {
        match route() {
            #[cfg(feature = "tracing")]
            Route::Tracing => tracing::warn!(
                target: TARGET,
                code = diagnostic.code.as_str(),
                scope = diagnostic.scope.label(),
                pointer = diagnostic.pointer.as_str(),
                event_type = diagnostic.wire_type(),
                run_id = diagnostic.run_id.as_ref().map(RunId::as_str),
                "{}",
                diagnostic
            ),
            #[cfg(feature = "log")]
            Route::Log => log::warn!(target: TARGET, "{}", diagnostic),
            Route::Stderr => {
                // A failed write to standard error has nowhere left to go.
                let _ = write_line(&mut std::io::stderr().lock(), diagnostic);
            }
        }
    }
}

/// Discards everything. The only way to silence diagnostics (VER-10).
#[derive(Debug, Clone, Copy, Default)]
pub struct NullSink;

impl DiagnosticSink for NullSink {
    fn report(&self, _diagnostic: &Diagnostic) {}

    fn enabled(&self) -> bool {
        false
    }
}

/// Collects diagnostics in memory, for tests and for
/// [`RunReport::diagnostics`][crate::client::RunReport::diagnostics].
#[cfg_attr(
    not(feature = "client"),
    doc = "",
    doc = "[crate::client::RunReport::diagnostics]: https://docs.rs/agui-sdk/latest/agui_sdk/client/struct.RunReport.html#structfield.diagnostics"
)]
#[derive(Debug, Default)]
pub struct CollectingSink {
    collected: Mutex<Vec<Diagnostic>>,
}

impl CollectingSink {
    /// An empty collector.
    #[must_use]
    pub fn new() -> Self {
        CollectingSink {
            collected: Mutex::new(Vec::new()),
        }
    }

    /// Takes everything collected so far, leaving the collector empty.
    pub fn take(&self) -> Vec<Diagnostic> {
        std::mem::take(&mut *self.lock())
    }

    /// Copies everything collected so far.
    #[must_use]
    pub fn snapshot(&self) -> Vec<Diagnostic> {
        self.lock().clone()
    }

    /// The collection, recovered if a previous holder panicked: a vector of
    /// complete diagnostics has no invariant a panic could break.
    fn lock(&self) -> std::sync::MutexGuard<'_, Vec<Diagnostic>> {
        self.collected
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }
}

impl DiagnosticSink for CollectingSink {
    fn report(&self, diagnostic: &Diagnostic) {
        self.lock().push(diagnostic.clone());
    }
}

/// Sends every diagnostic to both sinks.
#[derive(Debug, Clone, Copy, Default)]
pub struct Tee<A, B>(pub A, pub B);

impl<A: DiagnosticSink, B: DiagnosticSink> DiagnosticSink for Tee<A, B> {
    fn report(&self, diagnostic: &Diagnostic) {
        self.0.report(diagnostic);
        self.1.report(diagnostic);
    }

    fn enabled(&self) -> bool {
        self.0.enabled() || self.1.enabled()
    }
}

/// The shared default sink ([`DefaultSink`]).
///
/// The `Arc` is created once per process; later calls only bump its count.
#[must_use]
pub fn default_sink() -> Arc<dyn DiagnosticSink> {
    static SHARED: OnceLock<Arc<dyn DiagnosticSink>> = OnceLock::new();
    Arc::clone(SHARED.get_or_init(|| Arc::new(DefaultSink)))
}

/// Per-event rate limiter in front of a sink, used by every pipeline stage.
///
/// Stages call [`Reporter::report`] with a closure that builds the diagnostic,
/// so nothing is allocated once the cap is reached. [`Reporter::end_event`]
/// emits the `diagnostics.limit` summary if needed and resets the count.
pub struct Reporter {
    sink: Arc<dyn DiagnosticSink>,
    /// [`DiagnosticSink::enabled`], asked once: when `false` nothing is
    /// built, but every count moves as if it were delivered.
    enabled: bool,
    max_per_event: usize,
    count: usize,
    suppressed: usize,
}

impl Reporter {
    /// A reporter over `sink` with a per-event cap. A cap of 0 suppresses
    /// every diagnostic, leaving only the per-event summary.
    pub fn new(sink: Arc<dyn DiagnosticSink>, max_per_event: usize) -> Self {
        Reporter {
            enabled: sink.enabled(),
            sink,
            max_per_event,
            count: 0,
            suppressed: 0,
        }
    }

    /// Reports one diagnostic, built lazily.
    ///
    /// Under the cap, `build` runs and its result goes to the sink. At the
    /// cap, `build` is not called and only a counter moves, so a flood costs
    /// no allocation. Nor is it called for a sink that is not
    /// [`enabled`](DiagnosticSink::enabled).
    pub fn report(&mut self, build: impl FnOnce() -> Diagnostic) {
        if self.count < self.max_per_event {
            self.count += 1;
            if self.enabled {
                self.sink.report(&build());
            }
        } else {
            self.suppressed = self.suppressed.saturating_add(1);
        }
    }

    /// How many more diagnostics this event would deliver: 0 at the cap or
    /// for a sink that is not enabled. A stage that knows this up front can
    /// skip building what would never be delivered.
    pub(crate) fn room(&self) -> usize {
        if self.enabled {
            self.max_per_event.saturating_sub(self.count)
        } else {
            0
        }
    }

    /// Counts a diagnostic its stage did not build because [`room`] said
    /// it would not be delivered: the counts move exactly as
    /// [`report`](Self::report)'s would.
    ///
    /// [`room`]: Self::room
    pub(crate) fn report_unbuilt(&mut self) {
        if self.count < self.max_per_event {
            debug_assert!(!self.enabled, "an unbuilt diagnostic had room");
            self.count += 1;
        } else {
            self.suppressed = self.suppressed.saturating_add(1);
        }
    }

    /// Closes the current event's budget, reporting a summary if anything was
    /// suppressed.
    ///
    /// The summary has code [`DiagnosticCode::Limit`] and carries
    /// `event_type` (as [`Diagnostic::event_type`] when the type is known,
    /// else as [`Diagnostic::unknown_type`])
    /// when given, and does not count against the next event's budget.
    pub fn end_event(&mut self, event_type: Option<&str>) {
        let scope = if event_type.is_some() {
            Scope::Event
        } else {
            Scope::Stream
        };
        self.close(scope, event_type, "per-event cap");
    }

    /// As [`Reporter::end_event`] for a capability declaration: the summary
    /// has [`Scope::Capabilities`] like the entries it closes.
    pub(crate) fn end_capabilities(&mut self) {
        self.close(Scope::Capabilities, None, "per-declaration cap");
    }

    fn close(&mut self, scope: Scope, event_type: Option<&str>, cap_name: &str) {
        if self.suppressed > 0 && self.enabled {
            let mut summary = Diagnostic::new(
                DiagnosticCode::Limit,
                scope,
                "",
                format!(
                    "{} more diagnostics were suppressed ({} {})",
                    self.suppressed, cap_name, self.max_per_event
                ),
            );
            if let Some(wire_type) = event_type {
                match EventType::parse(wire_type) {
                    Some(known) => summary.event_type = Some(known),
                    None => summary = summary.with_unknown_type(wire_type),
                }
            }
            self.sink.report(&summary);
        }
        self.count = 0;
        self.suppressed = 0;
    }

    /// The underlying sink.
    #[must_use]
    pub fn sink(&self) -> &Arc<dyn DiagnosticSink> {
        &self.sink
    }
}

impl fmt::Debug for Reporter {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Reporter")
            .field("max_per_event", &self.max_per_event)
            .field("count", &self.count)
            .field("suppressed", &self.suppressed)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    const fn assert_send_sync<T: Send + Sync + ?Sized>() {}
    const _: () = {
        assert_send_sync::<Diagnostic>();
        assert_send_sync::<DiagnosticCode>();
        assert_send_sync::<Reporter>();
        assert_send_sync::<dyn DiagnosticSink>();
        assert_send_sync::<CollectingSink>();
        assert_send_sync::<Tee<DefaultSink, NullSink>>();
    };

    fn stripped() -> Diagnostic {
        Diagnostic::new(
            DiagnosticCode::UnknownField,
            Scope::Event,
            "/extra",
            "undescribed property stripped",
        )
        .with_unknown_type("VENDOR_EVENT")
    }

    #[test]
    fn codes_are_unique_and_dotted() {
        let mut seen = std::collections::BTreeSet::new();
        for &code in DiagnosticCode::ALL {
            assert!(seen.insert(code.as_str()), "duplicate {code}");
            assert_eq!(code.to_string(), code.as_str());
            assert!(code.as_str().contains('.'), "{code} is not dotted");
        }
    }

    #[test]
    fn display_names_code_scope_type_and_pointer() {
        assert_eq!(
            stripped().to_string(),
            "strip.unknown_field: undescribed property stripped \
             (event VENDOR_EVENT at /extra)"
        );
        let whole = Diagnostic::new(
            DiagnosticCode::CapabilitiesIgnored,
            Scope::Capabilities,
            "",
            "declaration ignored",
        );
        assert_eq!(
            whole.to_string(),
            "capabilities.ignored: declaration ignored (capabilities)"
        );
    }

    #[test]
    fn display_escapes_control_characters() {
        let forged = Diagnostic::new(DiagnosticCode::UnknownEvent, Scope::Event, "", "dropped")
            .with_unknown_type("X\nWARN fake: line");
        let text = forged.to_string();
        assert!(!text.contains('\n'), "{text}");
        assert!(text.contains("X\\nWARN fake: line"), "{text}");
    }

    #[test]
    fn display_escapes_line_separators_and_bidi_controls() {
        let forged = Diagnostic::new(
            DiagnosticCode::UnknownField,
            Scope::Event,
            "/a\u{2028}b\u{2029}c\u{202E}d\u{2066}e\u{200F}f",
            "stripped",
        );
        let text = forged.to_string();
        for c in ['\u{2028}', '\u{2029}', '\u{202E}', '\u{2066}', '\u{200F}'] {
            assert!(!text.contains(c), "{text:?}");
        }
        assert!(text.contains("/a\\u{2028}b\\u{2029}c\\u{202e}d"), "{text}");
        // Ordinary non-ASCII text is left alone.
        let plain = Diagnostic::new(DiagnosticCode::UnknownField, Scope::Event, "/é✓", "x");
        assert!(plain.to_string().contains("/é✓"));
    }

    #[test]
    fn message_is_capped_at_192_bytes_on_a_char_boundary() {
        let long = "é".repeat(200); // 400 bytes, 2 per char
        let d = Diagnostic::new(DiagnosticCode::UnknownField, Scope::Event, "", long);
        assert_eq!(d.message.len(), 192);
        let odd = format!("a{}", "é".repeat(200));
        let d = Diagnostic::new(DiagnosticCode::UnknownField, Scope::Event, "", odd);
        assert_eq!(d.message.len(), 191);
        assert!(d.message.ends_with('é'));
        let short = Diagnostic::new(DiagnosticCode::UnknownField, Scope::Event, "", "ok");
        assert_eq!(short.message, "ok");
    }

    #[test]
    fn collecting_sink_take_and_snapshot() {
        let sink = CollectingSink::new();
        sink.report(&stripped());
        assert_eq!(sink.snapshot(), vec![stripped()]);
        assert_eq!(sink.take(), vec![stripped()]);
        assert!(sink.take().is_empty());
    }

    #[test]
    fn tee_closure_and_arc_forward() {
        let left = Arc::new(CollectingSink::new());
        let right = Arc::new(CollectingSink::new());
        let right_in_closure = Arc::clone(&right);
        let tee = Tee(Arc::clone(&left), move |d: &Diagnostic| {
            right_in_closure.report(d)
        });
        tee.report(&stripped());
        assert_eq!(left.take().len(), 1);
        assert_eq!(right.take().len(), 1);
    }

    #[test]
    fn reporter_caps_per_event_without_building_and_summarises() {
        let sink = Arc::new(CollectingSink::new());
        let mut reporter = Reporter::new(sink.clone(), 2);
        let mut built = 0;
        for _ in 0..5 {
            reporter.report(|| {
                built += 1;
                stripped()
            });
        }
        assert_eq!(built, 2, "closures past the cap must not run");
        assert_eq!(sink.snapshot().len(), 2);
        reporter.end_event(None);
        let all = sink.take();
        assert_eq!(all.len(), 3);
        let summary = &all[2];
        assert_eq!(summary.code, DiagnosticCode::Limit);
        assert_eq!(summary.scope, Scope::Stream);
        assert!(summary.message.contains('3'), "{}", summary.message);

        // The budget resets, and an event under the cap gets no summary.
        reporter.report(stripped);
        reporter.end_event(None);
        assert_eq!(sink.take(), vec![stripped()]);
        assert!(format!("{reporter:?}").contains("max_per_event: 2"));
        assert!(Arc::ptr_eq(
            reporter.sink(),
            &(sink as Arc<dyn DiagnosticSink>)
        ));
    }

    #[test]
    fn display_names_known_type_and_run() {
        let d = Diagnostic::new(
            DiagnosticCode::UnknownField,
            Scope::Event,
            "/extra",
            "undescribed property stripped",
        )
        .with_event_type(EventType::TextMessageStart)
        .with_run_id(RunId::new("r-1"));
        assert_eq!(
            d.to_string(),
            "strip.unknown_field: undescribed property stripped \
             (event TEXT_MESSAGE_START run r-1 at /extra)"
        );
    }

    #[test]
    fn limit_summary_carries_the_event_type() {
        let sink = Arc::new(CollectingSink::new());
        let mut reporter = Reporter::new(sink.clone(), 0);
        reporter.report(stripped);
        reporter.end_event(Some("TEXT_MESSAGE_START"));
        reporter.report(stripped);
        reporter.end_event(Some("VENDOR_EVENT"));
        let all = sink.take();
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].event_type, Some(EventType::TextMessageStart));
        assert_eq!(all[0].scope, Scope::Event);
        assert_eq!(all[1].event_type, None);
        assert_eq!(all[1].unknown_type.as_deref(), Some("VENDOR_EVENT"));
    }

    #[test]
    fn default_sink_is_shared() {
        assert!(Arc::ptr_eq(&default_sink(), &default_sink()));
    }

    /// covers: VER-10
    #[test]
    fn null_sink_silences_only_where_it_is_installed() {
        let seen = Arc::new(CollectingSink::new());
        let mut silenced = Reporter::new(Arc::new(NullSink), DEFAULT_MAX_PER_EVENT);
        let mut built = false;
        silenced.report(|| {
            built = true;
            stripped()
        });
        silenced.end_event(None);
        assert!(!built, "nothing is built for a sink that discards it");
        Tee(NullSink, Arc::clone(&seen)).report(&stripped());
        assert_eq!(seen.take().len(), 1);
        // A tee or an `Arc` with a live half still builds.
        let mut teed = Reporter::new(
            Arc::new(Tee(NullSink, Arc::clone(&seen))),
            DEFAULT_MAX_PER_EVENT,
        );
        teed.report(stripped);
        assert_eq!(seen.take().len(), 1);
        assert!(!Arc::new(NullSink).enabled());
    }

    /// A reporter over a sink that is not enabled keeps the counts a live
    /// one would: the per-event cap and the summary behave the same, only
    /// nothing is built.
    #[test]
    fn a_disabled_sink_keeps_the_counts() {
        let mut quiet = Reporter::new(Arc::new(NullSink), 2);
        assert_eq!(quiet.room(), 0);
        for _ in 0..3 {
            quiet.report(|| unreachable!("not built for a disabled sink"));
        }
        assert_eq!((quiet.count, quiet.suppressed), (2, 1));
        quiet.report_unbuilt();
        assert_eq!((quiet.count, quiet.suppressed), (2, 2));
        quiet.end_event(None);
        assert_eq!((quiet.count, quiet.suppressed), (0, 0));

        let sink = Arc::new(CollectingSink::new());
        let mut live = Reporter::new(sink.clone(), 2);
        assert_eq!(live.room(), 2);
        live.report(stripped);
        assert_eq!(live.room(), 1);
        live.report(stripped);
        live.report_unbuilt();
        assert_eq!(live.room(), 0);
        live.end_event(None);
        let all = sink.take();
        assert_eq!(all.len(), 3);
        assert!(all[2].message.starts_with("1 more"), "{}", all[2].message);
    }

    #[test]
    fn stderr_line_is_one_line_with_target() {
        let mut out = Vec::new();
        assert!(write_line(&mut out, &stripped()).is_ok());
        assert_eq!(
            String::from_utf8_lossy(&out),
            "WARN agui_sdk::diagnostics: strip.unknown_field: undescribed property \
             stripped (event VENDOR_EVENT at /extra)\n"
        );
    }

    /// Counts the `write` calls it receives.
    struct CountingWriter {
        writes: usize,
        bytes: Vec<u8>,
    }

    impl Write for CountingWriter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.writes += 1;
            self.bytes.extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn stderr_line_is_one_write() {
        let diagnostic = Diagnostic::new(
            DiagnosticCode::UnknownField,
            Scope::Event,
            "/a\u{1b}b",
            "two\nlines\u{7}",
        )
        .with_unknown_type("X\nY");
        let mut out = CountingWriter {
            writes: 0,
            bytes: Vec::new(),
        };
        assert!(write_line(&mut out, &diagnostic).is_ok());
        assert_eq!(out.writes, 1, "one syscall per line");
        let expected = format!("WARN {TARGET}: {diagnostic}\n");
        assert_eq!(String::from_utf8_lossy(&out.bytes), expected);
    }

    #[test]
    fn end_event_bounds_the_unknown_type_preview() {
        let sink = Arc::new(CollectingSink::new());
        let mut reporter = Reporter::new(sink.clone(), 0);
        reporter.report(stripped);
        reporter.end_event(Some(&"Z".repeat(200)));
        let seen = sink.take();
        let Some(summary) = seen.last() else {
            panic!("a summary is reported")
        };
        let t = summary.unknown_type.as_deref().unwrap_or_default();
        assert!(
            t.len() <= MAX_UNKNOWN_TYPE_BYTES + CUT.len(),
            "{} bytes",
            t.len()
        );
        assert!(t.ends_with('…'), "{t}");
    }

    /// Environment variable that turns [`default_sink_child`] into the child
    /// half of [`default_sink_is_never_silent`].
    const CHILD: &str = "AGUI_SDK_DIAGNOSTICS_CHILD";

    /// Child process body: reports one diagnostic through the default sink,
    /// with no tracing subscriber and (unless asked) no logger installed.
    #[test]
    fn default_sink_child() {
        let Ok(mode) = std::env::var(CHILD) else {
            return;
        };
        #[cfg(feature = "log")]
        if mode == "log" {
            static LOGGER: StderrLogger = StderrLogger;
            assert!(log::set_logger(&LOGGER).is_ok());
            log::set_max_level(log::LevelFilter::Warn);
            assert_eq!(route(), Route::Log);
        }
        if mode == "plain" {
            assert_eq!(route(), Route::Stderr);
        }
        default_sink().report(&stripped());
    }

    /// A logger that tags its output, so the parent can tell the routes apart.
    #[cfg(feature = "log")]
    struct StderrLogger;

    #[cfg(feature = "log")]
    impl log::Log for StderrLogger {
        fn enabled(&self, metadata: &log::Metadata<'_>) -> bool {
            metadata.target() == TARGET
        }
        fn log(&self, record: &log::Record<'_>) {
            eprintln!("LOGGER {}", record.args());
        }
        fn flush(&self) {}
    }

    fn run_child(mode: &str) -> String {
        let exe = std::env::current_exe();
        let Ok(exe) = exe else {
            panic!("no test executable path");
        };
        let output = Command::new(exe)
            .args([
                "--exact",
                "diagnostics::tests::default_sink_child",
                "--nocapture",
                "--test-threads=1",
            ])
            .env(CHILD, mode)
            .output();
        let Ok(output) = output else {
            panic!("could not run the child test process");
        };
        assert!(output.status.success(), "child failed: {output:?}");
        String::from_utf8_lossy(&output.stderr).into_owned()
    }

    /// covers: VER-11
    #[test]
    fn default_sink_is_never_silent() {
        if std::env::var_os(CHILD).is_some() {
            return;
        }
        let stderr = run_child("plain");
        assert!(
            stderr.contains("WARN agui_sdk::diagnostics: strip.unknown_field"),
            "nothing reached standard error: {stderr:?}"
        );
        #[cfg(feature = "log")]
        {
            let logged = run_child("log");
            assert!(
                logged.contains("LOGGER strip.unknown_field"),
                "the log route was not taken: {logged:?}"
            );
            assert!(!logged.contains("WARN agui_sdk::diagnostics"), "{logged:?}");
        }
    }

    #[cfg(feature = "tracing")]
    mod tracing_route {
        use super::*;
        use std::sync::atomic::{AtomicUsize, Ordering};
        use tracing::span::{Attributes, Id, Record};
        use tracing::{Event, Metadata, Subscriber};

        /// Counts WARN events on [`TARGET`]; interested in nothing else.
        struct Counting(Arc<AtomicUsize>);

        impl Subscriber for Counting {
            fn enabled(&self, metadata: &Metadata<'_>) -> bool {
                metadata.target() == TARGET && *metadata.level() == tracing::Level::WARN
            }
            fn new_span(&self, _: &Attributes<'_>) -> Id {
                Id::from_u64(1)
            }
            fn record(&self, _: &Id, _: &Record<'_>) {}
            fn record_follows_from(&self, _: &Id, _: &Id) {}
            fn event(&self, event: &Event<'_>) {
                if event.metadata().target() == TARGET {
                    self.0.fetch_add(1, Ordering::SeqCst);
                }
            }
            fn enter(&self, _: &Id) {}
            fn exit(&self, _: &Id) {}
        }

        /// Refuses everything, like a subscriber filtered to errors only.
        struct Uninterested;

        impl Subscriber for Uninterested {
            fn enabled(&self, _: &Metadata<'_>) -> bool {
                false
            }
            fn new_span(&self, _: &Attributes<'_>) -> Id {
                Id::from_u64(1)
            }
            fn record(&self, _: &Id, _: &Record<'_>) {}
            fn record_follows_from(&self, _: &Id, _: &Id) {}
            fn event(&self, _: &Event<'_>) {}
            fn enter(&self, _: &Id) {}
            fn exit(&self, _: &Id) {}
        }

        /// covers: VER-11
        #[test]
        fn interested_subscriber_takes_diagnostics() {
            let count = Arc::new(AtomicUsize::new(0));
            tracing::subscriber::with_default(Counting(Arc::clone(&count)), || {
                assert_eq!(route(), Route::Tracing);
                DefaultSink.report(&stripped());
            });
            assert_eq!(count.load(Ordering::SeqCst), 1);
        }

        /// covers: VER-11
        #[test]
        fn uninterested_subscriber_falls_through() {
            tracing::subscriber::with_default(Uninterested, || {
                assert_ne!(route(), Route::Tracing);
            });
        }
    }
}
