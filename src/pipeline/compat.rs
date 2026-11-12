//! The compatibility boundary and the era shims: the stages next to the wire.
//!
//! # The boundary
//!
//! [`Boundary`] is always on and sees every received frame before anything
//! else does (PROC-22): a retired shape can be a chunk, and a later stage
//! would either fail on it or turn it into something no longer recognisable.
//! It translates the shapes the deprecation registry (`DEPRECATIONS.md`
//! upstream) lists as retired in 1.0:
//!
//! * the five `THINKING_*` events become their `REASONING_*` replacements
//!   (VER-13). 0.x thinking events carried no identifiers, so the boundary
//!   names each span and each reasoning message it opens; an identifier the
//!   frame already carries is kept as is;
//! * a legacy `{"type": "binary"}` content part becomes the media part its
//!   MIME type names (`image`, `audio`, `video`, otherwise `document`), with a
//!   `data`, `url` or `file` source. A `filename` moves to the part's
//!   `metadata`. A legacy part that only mirrors a modern part already in the
//!   same list is removed, the modern part kept;
//! * a whole optional field sent as `null`, in the positions the registry
//!   lists, is read as absent (BASIC-12). The list and how it was derived are
//!   documented on the private `absent` module; a `null` anywhere else, and
//!   any `null` inside open data (metadata values, state), is left for
//!   enforcement to judge or keep.
//!
//! Inbound, each kind of translation is reported once per run, naming the
//! retired shape and what the producer should send instead, so the developer
//! learns that a producer depends on a shim with an expiry date. A translation
//! that loses content (a thinking span's `title`, the second source of a
//! legacy part that carried two) warns every time (VER-15). A conformant 1.0
//! frame draws nothing (PROC-37).
//!
//! The boundary never repairs: a frame it cannot translate faithfully (a
//! thinking fragment with no open message, a legacy part with no MIME type)
//! is passed on unchanged or with the identifier left absent, so enforcement
//! or verification reports the defect exactly as it would for a 1.0 producer.
//!
//! [`Boundary::upgrade_input`] applies the same content and null rules to a
//! run input: on the client before the input is enforced and sent (legacy
//! binary parts are upgraded for every peer, silently, because the upgrade
//! loses nothing). Server admission does not apply the legacy-binary content
//! upgrade, as in the reference implementations, where request parsing does
//! not pass through the event boundary; its enforcement does read the
//! registry's input nulls as absent (D9), so `upgrade_input` is needed there
//! only to accept legacy binary parts. A server that accepts them can check
//! the body size, parse it to a `Value`, call `upgrade_input`, serialize it
//! back to bytes and pass those to `server::input::admit`, so size limits,
//! enforcement and diagnostics still run. `admit_input` applies the version
//! policy only and is not a substitute for enforcing a hostile body.
//!
//! # The era shims
//!
//! [`EraShims`] apply only when the application pinned a peer that predates
//! versioning ([`LegacyEra`], VER-03, VER-30); an unpinned peer is treated as
//! current (VER-31), and code that holds no `EraShims` pays nothing. They sit
//! between the boundary and user middleware, so middleware always sees 1.0
//! shapes (VER-14). The eras are cumulative: a peer pinned at 0.0.39 also
//! gets every shim of the later eras.
//!
//! | Peer at or below | Outgoing input | Events |
//! |---|---|---|
//! | 0.0.57 | `protocolVersion` removed (RUNIN-08); `subagentRunId` stripped from messages (silent, VER-06) | `SUBAGENT_*` dropped with a warning (VER-07, VER-08); `subagentRunId` stripped (silent) |
//! | 0.0.47 | as 0.0.57 (media parts are sent as they are; the fixtures pin this) | as 0.0.57; producer side, media parts take the legacy `binary` shape; on the typed `Runner` path, dropped with a warning |
//! | 0.0.39 | user and tool content parts flattened to their text, a warning per dropped part, no placeholder | as 0.0.57 |
//!
//! [`EraShims::downgrade_event`] is the producer-side counterpart for a
//! server writing to a reader pinned to an era: the same subagent rule, and,
//! in tool results and in the messages a snapshot or an input echo carries,
//! legacy `binary` parts for 0.0.47 and flattening for 0.0.39.
//! [`EraShims::for_pin`] maps a client's [`PeerPin`] to its shims.
//!
//! Every shim is a pure transformation (VER-32): it removes or reshapes what
//! is there, never invents a value (an empty string where every part was
//! dropped is reshaping, VER-04), never repairs a malformed one (content with a
//! malformed part is left whole for enforcement to reject, VER-05), never
//! touches an identifier and never reorders. Dropping an event is the only
//! way a shim changes the sequence, and it always warns. Warnings go through
//! the [`Reporter`] they are given; nothing here silences them (VER-11), and
//! only a [`NullSink`](crate::diagnostics::NullSink) behind the reporter does
//! (VER-10).

use serde_json::{Map, Value};

use crate::diagnostics::{Diagnostic, DiagnosticCode, Reporter, Scope};
use crate::event::EventType;
use crate::version::{LegacyEra, PeerPin};

mod absent;
mod media;

pub(crate) use absent::listed as null_is_listed;
mod shims;
mod thinking;

/// The always-on compatibility boundary.
///
/// One boundary per stream. It keeps the open thinking spans and messages it
/// named, and which once-per-run notices it has already given; both reset on
/// `RUN_STARTED`. A span closes when a `THINKING_END` or a 1.0
/// `REASONING_END` names it, so the boundary must see every frame of the
/// stream (or at least every `REASONING_END`) to keep only open spans.
#[derive(Debug, Default)]
pub struct Boundary {
    thinking: thinking::Thinking,
    noticed: u64,
}

impl Boundary {
    /// A boundary for one stream.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Translates one frame in place. Returns `false` when the frame was
    /// consumed by a translation (and must not continue down the pipeline).
    ///
    /// Every translation the boundary performs today maps one frame to one
    /// frame, so this currently always returns `true`. A frame that is not an
    /// object, or has no string `type`, is passed on untouched for
    /// enforcement to judge.
    ///
    /// On a 1.0 frame this is a type dispatch plus a few member lookups, and
    /// allocates nothing.
    pub fn upgrade(&mut self, frame: &mut Value, reporter: &mut Reporter) -> bool {
        let Value::Object(map) = frame else {
            return true;
        };
        let kind = match map.get("type") {
            Some(Value::String(name)) => Kind::of(name),
            _ => return true,
        };
        if kind == Kind::RunStarted {
            self.run_started(map.get("runId").and_then(Value::as_str));
        }
        // Event types the boundary does not dispatch on are only looked up
        // when a retired null is about to be reported (the slow path).
        let event_type = match kind.event_type() {
            Some(known) => Some(known),
            None if absent::has_event_null(map, kind) => map
                .get("type")
                .and_then(Value::as_str)
                .and_then(EventType::parse),
            None => None,
        };
        let mut notes = Notes {
            reporter,
            scope: Scope::Event,
            event_type,
            noticed: &mut self.noticed,
            inbound: true,
        };
        absent::event(map, kind, &mut notes);
        match kind {
            Kind::Thinking(step) => self.thinking.translate(step, map, &mut notes),
            Kind::RunStarted => {
                if let Some(Value::Object(input)) = map.get_mut("input") {
                    input_content(input, &Path::ROOT.key("input"), &mut notes);
                }
            }
            Kind::MessagesSnapshot => {
                if let Some(Value::Array(messages)) = map.get_mut("messages") {
                    messages_content(messages, &Path::ROOT.key("messages"), &mut notes);
                }
            }
            Kind::ToolCallResult => {
                if let Some(Value::Array(parts)) = map.get_mut("content") {
                    media::parts(parts, &Path::ROOT.key("content"), &mut notes);
                }
            }
            // A 1.0 end of a span the shim opened closes it in the shim too,
            // so mixing the families leaves no entry behind (D28).
            Kind::Other if self.thinking.has_open_spans() => {
                if let (Some(Value::String(t)), Some(Value::String(id))) =
                    (map.get("type"), map.get("messageId"))
                {
                    if t == "REASONING_END" {
                        self.thinking.close_named(id);
                    }
                }
            }
            _ => {}
        }
        true
    }

    /// Whether a 1.0 frame naming `REASONING_END` must still pass through
    /// the boundary: while a translated span is open, so its end closes it.
    pub(crate) fn tracks_spans(&self) -> bool {
        self.thinking.has_open_spans()
    }

    /// Estimated resident bytes the boundary keeps under peer-chosen ids
    /// (the open thinking spans and message it tracks), which a processor
    /// charges to the history budget with the rest of its id bookkeeping
    /// (D25, D28).
    pub(crate) fn retained(&self) -> usize {
        self.thinking.retained()
    }

    /// A new run began: nothing is open, the once-per-run notices may be
    /// given again, and names minted from now on use `run_id`. `upgrade`
    /// calls this for each `RUN_STARTED` it sees; the processor's fast path
    /// calls it for a `RUN_STARTED` that did not need the boundary.
    pub(crate) fn run_started(&mut self, run_id: Option<&str>) {
        self.noticed = 0;
        self.thinking.run_started(run_id);
    }

    /// Upgrades a run input in place: legacy binary parts become media parts
    /// and the input positions of the optional-null list are read as absent.
    ///
    /// The client calls this on every outgoing input, whatever the peer, before
    /// [`EraShims::downgrade_input`] and enforcement; the binary upgrade is
    /// lossless there and therefore silent. Server admission does not call
    /// it, so it does not upgrade legacy binary parts (its enforcement
    /// already reads the registry's input nulls as absent, D9); a server
    /// that accepts older clients' binary parts may call it on the
    /// parsed body (after the size check) and serialize the result back into
    /// `server::input::admit`, which still enforces the input. Only
    /// lossy steps and `null` reads are reported, with [`Scope::RunInput`].
    pub fn upgrade_input(input: &mut Value, reporter: &mut Reporter) {
        let Value::Object(map) = input else {
            return;
        };
        let mut noticed = 0;
        let mut notes = Notes {
            reporter,
            scope: Scope::RunInput,
            event_type: None,
            noticed: &mut noticed,
            inbound: false,
        };
        input_content(map, &Path::ROOT, &mut notes);
    }
}

/// Upgrade and downgrade shims for one pinned legacy era.
#[derive(Debug, Clone)]
pub struct EraShims {
    era: LegacyEra,
}

impl EraShims {
    /// Shims for `era`.
    #[must_use]
    pub fn new(era: LegacyEra) -> Self {
        Self { era }
    }

    /// The shims a client pin calls for: `Some` only for
    /// [`PeerPin::Legacy`]; a current or versioned peer needs none (VER-31).
    #[must_use]
    pub fn for_pin(pin: PeerPin) -> Option<Self> {
        match pin {
            PeerPin::Legacy(era) => Some(Self::new(era)),
            _ => None,
        }
    }

    /// The era these shims target.
    #[must_use]
    pub fn era(&self) -> LegacyEra {
        self.era
    }

    /// Upgrades an incoming frame from the era's shape to 1.0. Returns
    /// `false` to drop the frame (e.g. subagent events for a pre-0.0.58
    /// reader, with a warning, VER-07, VER-08).
    ///
    /// For every era (all predate 0.0.58) `SUBAGENT_*` events are dropped
    /// with a warning and `subagentRunId` attribution is stripped from the
    /// survivors without one (VER-06): the events still arrive, in one
    /// thread.
    pub fn upgrade_event(&self, frame: &mut Value, reporter: &mut Reporter) -> bool {
        shims::event(self.era, frame, reporter, shims::Direction::Inbound)
    }

    /// Downgrades an outgoing event for a reader pinned to the era: the
    /// producer-side counterpart of [`EraShims::upgrade_event`], for a server
    /// configured to serve legacy clients. Returns `false` to drop the event.
    ///
    /// Subagent events are dropped with a warning and attribution is
    /// stripped silently (VER-06..VER-09). In the content of tool results,
    /// of snapshot messages and of the input echoed on `RUN_STARTED`, media
    /// parts take the legacy `binary` shape for 0.0.47 (a part without a MIME
    /// type, which that shape requires, is dropped with a warning), and for
    /// 0.0.39 parts are flattened to their text, warning for each dropped part
    /// (TOOL-22). `RUN_STARTED.protocolVersion` is kept: a producer declares
    /// its own version (VER-18). A pure transformation: no invented values,
    /// ids and order untouched (VER-04, VER-05, VER-32).
    pub fn downgrade_event(&self, frame: &mut Value, reporter: &mut Reporter) -> bool {
        shims::event(self.era, frame, reporter, shims::Direction::Outbound)
    }

    /// [`EraShims::downgrade_event`] for a caller that reads the result back
    /// into a typed `Event`, which cannot hold the legacy `binary` part: for
    /// 0.0.47 a media part is dropped with a warning instead of rewritten.
    /// Everything else is as `downgrade_event`.
    #[cfg(feature = "server")]
    pub(crate) fn downgrade_typed_event(&self, frame: &mut Value, reporter: &mut Reporter) -> bool {
        shims::event(self.era, frame, reporter, shims::Direction::OutboundTyped)
    }

    /// Downgrades outgoing run input to the era's shape: flattens content
    /// parts for 0.0.39 (lossy, warns), removes `protocolVersion` (RUNIN-08).
    /// A pure transformation: no invented values, no repaired ones, ids and
    /// order untouched (VER-04, VER-05, VER-32).
    pub fn downgrade_input(&self, input: &mut Value, reporter: &mut Reporter) {
        shims::input(self.era, input, reporter);
    }
}

/// When the registry's shims for 0.x shapes expire (twelve months after 1.0).
/// After it a release may remove them, and the shapes become unrecognised
/// like anything else (VER-16).
const SHIM_EXPIRY: &str = "2027-09-17";

/// Removes `key` keeping the order of the other members (with
/// `serde_json/preserve_order`, a plain `remove` would swap the last member
/// into its place).
fn remove_key(map: &mut Map<String, Value>, key: &str) -> Option<Value> {
    let mut taken = None;
    if map.contains_key(key) {
        map.retain(|name, value| {
            if name == key {
                taken = Some(std::mem::take(value));
                false
            } else {
                true
            }
        });
    }
    taken
}

/// Upgrades the content and optional nulls of a run input object (outgoing,
/// received, or echoed on `RUN_STARTED.input`).
fn input_content(input: &mut Map<String, Value>, path: &Path<'_>, notes: &mut Notes<'_, '_>) {
    absent::input(input, path, notes);
    if let Some(Value::Array(messages)) = input.get_mut("messages") {
        messages_content(messages, &path.key("messages"), notes);
    }
}

/// Upgrades the content parts of every message in a list.
fn messages_content(messages: &mut [Value], path: &Path<'_>, notes: &mut Notes<'_, '_>) {
    for (index, message) in messages.iter_mut().enumerate() {
        if let Some(Value::Array(parts)) = message.get_mut("content") {
            let at = path.index(index);
            media::parts(parts, &at.key("content"), notes);
        }
    }
}

/// The event types the boundary treats specially.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Kind {
    RunStarted,
    RunFinished,
    RunError,
    ToolCallStart,
    ToolCallChunk,
    ToolCallResult,
    MessagesSnapshot,
    SubagentStarted,
    SubagentFinished,
    SubagentError,
    Thinking(thinking::Step),
    Other,
}

impl Kind {
    fn of(name: &str) -> Self {
        match name {
            "RUN_STARTED" => Kind::RunStarted,
            "RUN_FINISHED" => Kind::RunFinished,
            "RUN_ERROR" => Kind::RunError,
            "TOOL_CALL_START" => Kind::ToolCallStart,
            "TOOL_CALL_CHUNK" => Kind::ToolCallChunk,
            "TOOL_CALL_RESULT" => Kind::ToolCallResult,
            "MESSAGES_SNAPSHOT" => Kind::MessagesSnapshot,
            "SUBAGENT_STARTED" => Kind::SubagentStarted,
            "SUBAGENT_FINISHED" => Kind::SubagentFinished,
            "SUBAGENT_ERROR" => Kind::SubagentError,
            other => thinking::Step::of(other).map_or(Kind::Other, Kind::Thinking),
        }
    }

    /// The typed event type for diagnostics: the replacement for a retired
    /// thinking event; `None` for [`Kind::Other`].
    fn event_type(self) -> Option<EventType> {
        Some(match self {
            Kind::RunStarted => EventType::RunStarted,
            Kind::RunFinished => EventType::RunFinished,
            Kind::RunError => EventType::RunError,
            Kind::ToolCallStart => EventType::ToolCallStart,
            Kind::ToolCallChunk => EventType::ToolCallChunk,
            Kind::ToolCallResult => EventType::ToolCallResult,
            Kind::MessagesSnapshot => EventType::MessagesSnapshot,
            Kind::SubagentStarted => EventType::SubagentStarted,
            Kind::SubagentFinished => EventType::SubagentFinished,
            Kind::SubagentError => EventType::SubagentError,
            Kind::Thinking(step) => step.replacement_type(),
            Kind::Other => return None,
        })
    }

    /// The wire name, for every kind but [`Kind::Other`].
    fn wire_name(self) -> Option<&'static str> {
        Some(match self {
            Kind::RunStarted => "RUN_STARTED",
            Kind::RunFinished => "RUN_FINISHED",
            Kind::RunError => "RUN_ERROR",
            Kind::ToolCallStart => "TOOL_CALL_START",
            Kind::ToolCallChunk => "TOOL_CALL_CHUNK",
            Kind::ToolCallResult => "TOOL_CALL_RESULT",
            Kind::MessagesSnapshot => "MESSAGES_SNAPSHOT",
            Kind::SubagentStarted => "SUBAGENT_STARTED",
            Kind::SubagentFinished => "SUBAGENT_FINISHED",
            Kind::SubagentError => "SUBAGENT_ERROR",
            Kind::Thinking(step) => step.retired_name(),
            Kind::Other => return None,
        })
    }

    fn is_subagent_lifecycle(self) -> bool {
        matches!(
            self,
            Kind::SubagentStarted | Kind::SubagentFinished | Kind::SubagentError
        )
    }
}

/// One step of an RFC 6901 pointer.
#[derive(Debug, Clone, Copy)]
enum Seg {
    Key(&'static str),
    Index(usize),
}

/// A pointer built on the stack while walking a document; rendered to a
/// string only when a diagnostic is actually reported. Keys are protocol
/// field names, which need no escaping.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Path<'a> {
    parent: Option<&'a Path<'a>>,
    seg: Option<Seg>,
}

impl<'a> Path<'a> {
    const ROOT: Path<'static> = Path {
        parent: None,
        seg: None,
    };

    fn key(&'a self, key: &'static str) -> Path<'a> {
        Path {
            parent: Some(self),
            seg: Some(Seg::Key(key)),
        }
    }

    fn index(&'a self, index: usize) -> Path<'a> {
        Path {
            parent: Some(self),
            seg: Some(Seg::Index(index)),
        }
    }

    fn render(&self) -> String {
        let mut out = String::new();
        self.render_into(&mut out);
        out
    }

    fn render_into(&self, out: &mut String) {
        use std::fmt::Write as _;
        if let Some(parent) = self.parent {
            parent.render_into(out);
        }
        match self.seg {
            Some(Seg::Key(key)) => {
                out.push('/');
                out.push_str(key);
            }
            Some(Seg::Index(index)) => {
                let _ = write!(out, "/{index}");
            }
            None => {}
        }
    }
}

/// Once-per-run notices, by bit in [`Boundary::noticed`].
#[derive(Debug, Clone, Copy)]
enum Notice {
    /// One bit per optional-null rule.
    Null(absent::Rule),
    /// One bit per retired thinking event.
    Thinking(thinking::Step),
    /// Legacy binary content upgraded.
    LegacyContent,
}

impl Notice {
    fn bit(self) -> u64 {
        let index = match self {
            Notice::Null(rule) => rule as u32,
            Notice::Thinking(step) => absent::Rule::COUNT + step as u32,
            Notice::LegacyContent => absent::Rule::COUNT + thinking::Step::COUNT,
        };
        1 << index
    }
}

/// What the walkers report through: the reporter plus where they are.
pub(crate) struct Notes<'r, 'n> {
    reporter: &'r mut Reporter,
    scope: Scope,
    /// The event type concerned; `None` for run input.
    event_type: Option<EventType>,
    noticed: &'n mut u64,
    /// Received events (notices are given) rather than an input (silent
    /// unless lossy).
    inbound: bool,
}

impl Notes<'_, '_> {
    fn build(
        scope: Scope,
        event_type: Option<EventType>,
        code: DiagnosticCode,
        path: &Path<'_>,
        message: String,
    ) -> Diagnostic {
        let diagnostic = Diagnostic::new(code, scope, path.render(), message);
        match event_type {
            Some(event_type) => diagnostic.with_event_type(event_type),
            None => diagnostic,
        }
    }

    /// Reports `message` unless `notice` was already given in this run.
    fn notice(
        &mut self,
        notice: Notice,
        code: DiagnosticCode,
        path: &Path<'_>,
        message: impl FnOnce() -> String,
    ) {
        let bit = notice.bit();
        if *self.noticed & bit != 0 {
            return;
        }
        *self.noticed |= bit;
        let (scope, event_type) = (self.scope, self.event_type);
        self.reporter
            .report(|| Self::build(scope, event_type, code, path, message()));
    }

    /// Reports a loss. Always, whatever was reported before (VER-07, VER-15).
    fn lossy(&mut self, code: DiagnosticCode, path: &Path<'_>, message: impl FnOnce() -> String) {
        let (scope, event_type) = (self.scope, self.event_type);
        self.reporter
            .report(|| Self::build(scope, event_type, code, path, message()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pointers_render_from_the_root() {
        let root = Path::ROOT;
        let input = root.key("input");
        let messages = input.key("messages");
        let second = messages.index(1);
        assert_eq!(second.render(), "/input/messages/1");
        assert_eq!(Path::ROOT.render(), "");
    }

    #[test]
    fn notice_bits_are_distinct() {
        let mut bits = 0u64;
        let mut all = Vec::new();
        all.extend(absent::Rule::ALL.iter().map(|rule| Notice::Null(*rule)));
        all.extend(
            thinking::Step::ALL
                .iter()
                .map(|step| Notice::Thinking(*step)),
        );
        all.push(Notice::LegacyContent);
        for notice in all {
            assert_eq!(bits & notice.bit(), 0, "{notice:?}");
            bits |= notice.bit();
        }
    }

    #[test]
    fn kinds_round_trip_their_wire_names() {
        for name in [
            "RUN_STARTED",
            "RUN_FINISHED",
            "RUN_ERROR",
            "TOOL_CALL_START",
            "TOOL_CALL_CHUNK",
            "TOOL_CALL_RESULT",
            "MESSAGES_SNAPSHOT",
            "SUBAGENT_STARTED",
            "SUBAGENT_FINISHED",
            "SUBAGENT_ERROR",
            "THINKING_START",
            "THINKING_END",
            "THINKING_TEXT_MESSAGE_START",
            "THINKING_TEXT_MESSAGE_CONTENT",
            "THINKING_TEXT_MESSAGE_END",
        ] {
            assert_eq!(Kind::of(name).wire_name(), Some(name));
        }
        assert_eq!(Kind::of("TEXT_MESSAGE_CONTENT"), Kind::Other);
        assert_eq!(Kind::Other.wire_name(), None);
    }
}
