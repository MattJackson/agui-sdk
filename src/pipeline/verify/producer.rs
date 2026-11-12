//! The producer-side verifier. See the [module docs](super).
//!
//! # What it refuses
//!
//! Every producer MUST / MUST NOT that a stream of typed events can break.
//! Rules the type system already makes impossible (an undescribed event type,
//! PROC-36; a `null` attribution, SUB-02; a reserved content part, TOOL-36; a
//! reasoning role other than `reasoning`, REAS-06; attribution on run-scoped
//! events, BASIC-08) have no check here.
//!
//! * **Lifecycle.** The first event is `RUN_STARTED` or `RUN_ERROR` (LIFE-01,
//!   LIFE-02). No `RUN_STARTED` inside a run (LIFE-44). After `RUN_FINISHED`
//!   only `RUN_STARTED` or a late `RUN_ERROR`; after `RUN_ERROR` only
//!   `RUN_STARTED` (LIFE-37, LIFE-41).
//! * **Identity.** Every run carries the stream's (and the input's) thread id;
//!   a run id is never reused on the stream; `RUN_FINISHED` names the run
//!   `RUN_STARTED` opened; nothing follows the requested run (BASIC-14,
//!   RUNIN-04..RUNIN-06, RUNIN-31). A declared `protocolVersion` is
//!   `MAJOR.MINOR`; under [`ProducerVerifier::new`] every run on the stream
//!   declares the same, while under [`ProducerVerifier::for_input`] each
//!   replayed run declares its own (VER-18, VER-28); none at all is legal
//!   only for a stream downgraded for a pre-versioning peer, which declares
//!   nothing on any run.
//! * **Items.** Text messages, tool calls, reasoning messages, reasoning spans
//!   and steps open once, continue and close only while open, and are all
//!   closed by `RUN_FINISHED` (STRM-01..STRM-03, REAS-02..REAS-04,
//!   LIFE-49..LIFE-51, LIFE-12). A closed message or call may reopen only
//!   with a `START` that agrees with it (TEXT-05, TEXT-06, TOOL-07): within
//!   its run on owner, role, name and parent; a text or reasoning message of
//!   the input history or a `MESSAGES_SNAPSHOT` on its kind, owner and role;
//!   a message of an earlier run on its kind, as its tracking has reset. A
//!   message id is used by one message in the thread, history and earlier
//!   runs included (BASIC-15).
//! * **Chunks.** The shorthand is tracked in lanes, one per owner, exactly as
//!   the consumer's chunk expander tracks it, so what this verifier accepts
//!   expands into a stream the consumer accepts. First chunks carry their
//!   opener fields (STRM-11, STRM-13, STRM-14), continuations repeat them
//!   unchanged (STRM-17, STRM-19), an item keeps one spelling (STRM-09), an
//!   id-less continuation continues only its own lane (STRM-25), and a chunk
//!   stream does not outlive its subagent (STRM-26).
//! * **Subagents and ownership.** Announcements are unique per run, precede
//!   attribution and name only announced parents; every announced invocation
//!   ends before the run does (SUB-03, SUB-07..SUB-10, SUB-28, SUB-33).
//!   Continuations agree with their opener's owner, steps finish under the
//!   attribution they started with, and a tool call agrees with its parent
//!   message's owner (SUB-18..SUB-22, SUB-26, TOOL-02). An untagged
//!   continuation continues whatever its opener owns (SUB-20), and an untagged
//!   tool call inherits its parent message's owner (SUB-26).
//! * **Outcomes.** A run that a subagent suspended does not end as success
//!   (INTR-02); interrupt lists are non-empty with unique ids (INTR-01,
//!   INTR-04); `pendingToolCallIds`, when sent and non-empty, lists exactly the unanswered
//!   calls in the order made (TOOL-29, TOOL-31); a cancelled run carries no
//!   result (LIFE-14); a frontend call is neither answered (TOOL-24) nor
//!   reported as an interrupt (TOOL-26).
//! * **Activity.** A delta needs a snapshot baseline (ACT-11, SNAP-07), agrees
//!   with the owner (SUB-21), and cannot make the content anything but an
//!   object (ACT-15); a `replace: false` snapshot agrees with the owner
//!   (ACT-08).
//! * **Values the types admit but the wire rejects** (ARCH-04, BASIC-09):
//!   `timestamp` and token counts beyond ±(2^53−1), a `null` `rawEvent` or
//!   `result`, a JSON Patch pointer that is not RFC 6901 or an operation
//!   missing its `value`/`from`, an empty `subagentRunId` (SUB-06), token
//!   usage whose parts exceed their totals or whose total is not input plus
//!   output (LIFE-26, LIFE-29).
//!
//! Frontend tools are the input's `tools` ([`ProducerVerifier::for_input`]),
//! or the `tools` of a `RUN_STARTED.input` echo, which also contributes its
//! history's message ids.
//!
//! # Check, then commit
//!
//! [`ProducerVerifier::check`] decides first and mutates only once the event
//! is accepted, so a refused event leaves the state exactly as it was, with
//! nothing cloned to make that so.
//!
//! # Cost
//!
//! O(1) per content event: every check is a hash lookup keyed by an id (an
//! `Arc<str>` newtype, so a key is a reference-count increment), and content
//! events allocate nothing. Openers are amortized O(1), allocating only when
//! a table grows. `MESSAGES_SNAPSHOT` and an echoed `RUN_STARTED.input` are
//! linear in the payload. Tool and text-message names are compared by
//! fingerprint; step names are stored.

use std::collections::{HashMap, HashSet};

use serde_json::Value;

use crate::diagnostics::shown;
use crate::event::Event;
use crate::ids::{InterruptId, MessageId, RunId, SubagentRunId, ThreadId, ToolCallId};
use crate::lifecycle::{RunFinishedOutcome, RunStarted, StepFinished};
use crate::message::Message;
use crate::patch::{PatchOp, PatchOperation};
use crate::pipeline::fingerprint;
use crate::reasoning::{ReasoningEnd, ReasoningMessageEnd};
use crate::run_input::RunAgentInput;
use crate::subagent::{SubagentFinished, SubagentFinishedOutcome};
use crate::text::{TextMessageEnd, TextMessageRole};
use crate::tool::ToolCallEnd;
use crate::usage::TokenUsage;
use crate::version::ProtocolVersion;

use super::consumer::{owner_text, quote_list, MAX_LISTED_IDS};
use super::RunPhase;

/// Largest integer JSON carries exactly: 2^53 − 1.
const MAX_SAFE_INTEGER: i64 = crate::json::MAX_SAFE_INTEGER as i64;

/// Who owns an item: `None` is the parent agent.
type Owner = Option<SubagentRunId>;

/// A producer rule the [`ProducerVerifier`] enforces.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ProducerRule {
    /// First event must be `RUN_STARTED` or `RUN_ERROR` (LIFE-01, LIFE-02).
    FirstEvent,
    /// Nothing after a closed run but `RUN_STARTED` or a late `RUN_ERROR` (LIFE-37, LIFE-41).
    AfterClose,
    /// No `RUN_STARTED` while a run is active (LIFE-44).
    NestedRun,
    /// An item opened twice while open (STRM-01, REAS-02, LIFE-49).
    AlreadyOpen,
    /// Content or end for an item that is not open, or a terminal for a
    /// subagent invocation that is not active (STRM-02, REAS-04, LIFE-50,
    /// SUB-10).
    NotOpen,
    /// Something still open at the terminal (STRM-03, REAS-03, LIFE-51, SUB-10, SUB-33).
    OpenAtFinish,
    /// Continuation attribution disagrees with the opener, or a reopening
    /// START disagrees with the item's role or name (SUB-18, SUB-19, SUB-22,
    /// SUB-26, TEXT-06).
    OwnerMismatch,
    /// Subagent announced twice, reused, or parent unknown (SUB-08, SUB-09, SUB-28).
    SubagentIdentity,
    /// Event attributed to an announced invocation before its announcement (SUB-07).
    AttributedBeforeAnnounced,
    /// Terminal ids disagree with `RUN_STARTED` or the input; a replayed run
    /// carries another thread id, or the requested run is not last (RUNIN-04,
    /// RUNIN-05, RUNIN-06, RUNIN-31).
    Identity,
    /// A `messageId` already used in the thread, by the input history or
    /// earlier in the stream (BASIC-15).
    DuplicateMessageId,
    /// A value the wire schema rejects: a JSON Patch pointer that is not RFC
    /// 6901, an integer outside ±(2^53−1) (`timestamp`, token counts), an
    /// open-JSON member that is `null` where absence is required (BASIC-09,
    /// ARCH-04), an empty `subagentRunId` (SUB-06), token usage whose parts
    /// exceed their totals or whose total is not input plus output (LIFE-26,
    /// LIFE-29), a `protocolVersion` that is not `MAJOR.MINOR` (VER-18), a
    /// patch operation missing its `value` or `from`, an activity delta
    /// leaving non-object content or moving or copying onto the activity root
    /// (ACT-15), or, without an input, a `protocolVersion` that differs from
    /// the one an earlier run on the stream declared (VER-28).
    InvalidValue,
    /// Chunk spelling mixed with explicit events within one item, or a missing
    /// first-chunk field (STRM-09, STRM-11..STRM-14, STRM-17).
    ChunkDiscipline,
    /// Outcome inconsistent with the run: success with open interrupts,
    /// pending ids not matching unanswered calls, empty interrupt list, a
    /// frontend call answered or reported as an interrupt, a cancelled run
    /// with a result (INTR-02, INTR-23, TOOL-24, TOOL-26, TOOL-31, LIFE-11,
    /// LIFE-14).
    Outcome,
    /// A delta before any baseline (SNAP-07, ACT-11).
    NoBaseline,
    /// Interrupt ids not unique in the run (INTR-04).
    DuplicateInterrupt,
    /// Not emitted by this version: the content types cannot express a
    /// reserved part (TOOL-36; see the module docs). Kept for compatibility.
    ReservedPart,
    /// Not emitted by this version: the `Runner` emits lifecycle events
    /// itself. Kept for compatibility.
    LifecycleFromAgent,
}

/// A refused event.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct ProducerViolation {
    /// The rule broken.
    pub rule: ProducerRule,
    /// Human-readable detail (ids, no payloads).
    pub message: String,
}

impl ProducerViolation {
    /// A violation of `rule`.
    pub fn new(rule: ProducerRule, message: impl Into<String>) -> Self {
        ProducerViolation {
            rule,
            message: message.into(),
        }
    }
}

impl std::fmt::Display for ProducerViolation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:?}: {}", self.rule, self.message)
    }
}

impl std::error::Error for ProducerViolation {}

type Verdict = std::result::Result<(), ProducerViolation>;

fn refuse(rule: ProducerRule, message: impl Into<String>) -> Verdict {
    Err(ProducerViolation::new(rule, message))
}

/// The producer-side verifier.
#[derive(Debug, Default)]
pub struct ProducerVerifier {
    state: ProducerState,
}

impl ProducerVerifier {
    /// A verifier for one response stream with no request: stream identity
    /// (one thread, no reused `runId`, matching terminals, BASIC-15 within
    /// the stream) is still checked, but nothing is checked against an
    /// input's ids, history or frontend tools. For tests of stream-local
    /// rules.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// A verifier for the response to `input`: terminals must carry the
    /// input's thread and run ids, replayed runs the input's thread id, the
    /// requested run must be the last on the stream, and new message ids must
    /// not collide with the input history (RUNIN-04..RUNIN-06, RUNIN-31,
    /// BASIC-15). The runs before the requested one are replay, so each
    /// declares its own `protocolVersion` (VER-28). What the runner uses.
    #[must_use]
    pub fn for_input(input: &RunAgentInput) -> Self {
        let mut state = ProducerState::default();
        state.register_history(&input.messages);
        state.thread = Some(input.thread_id.clone());
        state.input = Some(InputFacts {
            run_id: input.run_id.clone(),
            frontend: frontend_names(input),
        });
        ProducerVerifier { state }
    }

    /// Checks one event before it is written. A refused event does not change
    /// the verifier's state. Besides stream discipline it re-validates values
    /// the types admit but the wire rejects ([`ProducerRule::InvalidValue`]).
    ///
    /// # Errors
    ///
    /// The violation.
    pub fn check(&mut self, event: &Event) -> std::result::Result<(), ProducerViolation> {
        self.state.check(event)
    }

    /// The `*_END`/`STEP_FINISHED`/`SUBAGENT_FINISHED` events that would close
    /// everything still open, innermost first, for a runner that must close
    /// before a cancelling terminal (LIFE-12).
    pub fn closers(&self, out: &mut Vec<Event>) {
        self.state.closers(&mut |_| true, out);
    }

    /// As [`closers`](Self::closers), keeping only the items `keep` accepts.
    /// Rejected items are never built or sorted.
    #[cfg(feature = "server")]
    pub(crate) fn closers_where(
        &self,
        keep: &mut dyn FnMut(Holder<'_>) -> bool,
        out: &mut Vec<Event>,
    ) {
        self.state.closers(keep, out);
    }

    /// Tool calls started and not answered in this run, in the order made
    /// (for `pendingToolCallIds`, TOOL-29..TOOL-31); backend and frontend
    /// alike.
    #[must_use]
    pub fn unanswered_tool_calls(&self) -> Vec<crate::ids::ToolCallId> {
        self.state
            .calls_order
            .iter()
            .filter(|id| !self.state.answered.contains(*id))
            .cloned()
            .collect()
    }

    /// Current lifecycle phase.
    #[must_use]
    pub fn phase(&self) -> RunPhase {
        self.state.phase
    }
}

/// What [`ProducerVerifier::for_input`] knows about the request.
#[derive(Debug)]
struct InputFacts {
    /// The requested run.
    run_id: RunId,
    /// Fingerprints of the input's frontend tool names.
    frontend: HashSet<u64>,
}

/// What kind of message first used an id in the thread.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MessageKind {
    History,
    Snapshot,
    Text,
    Reasoning,
    ToolResult,
    Activity,
}

impl MessageKind {
    const fn noun(self) -> &'static str {
        match self {
            MessageKind::History => "a message in the input history",
            MessageKind::Snapshot => "a message restated by MESSAGES_SNAPSHOT",
            MessageKind::Text => "a text message",
            MessageKind::Reasoning => "a reasoning message",
            MessageKind::ToolResult => "a tool result message",
            MessageKind::Activity => "an activity message",
        }
    }
}

/// A text or reasoning message the input history or a `MESSAGES_SNAPSHOT`
/// holds: what a reopening `START` must agree with (TEXT-06), and the owner
/// a tool call naming it as parent inherits (SUB-26).
#[derive(Debug)]
struct Held {
    owner: Owner,
    /// `None`: a reasoning message.
    role: Option<TextMessageRole>,
}

impl Held {
    /// The held text or reasoning message `message` is, if it is one.
    fn of(message: &Message) -> Option<Held> {
        let role = match message {
            Message::Assistant(_) => Some(TextMessageRole::Assistant),
            Message::User(_) => Some(TextMessageRole::User),
            Message::System(_) => Some(TextMessageRole::System),
            Message::Developer(_) => Some(TextMessageRole::Developer),
            Message::Reasoning(_) => None,
            _ => return None,
        };
        Some(Held {
            owner: message.subagent_run_id().cloned(),
            role,
        })
    }
}

/// The `name` a restated text message carries.
fn restated_name(message: &Message) -> Option<&str> {
    match message {
        Message::Developer(m) => m.name.as_deref(),
        Message::System(m) => m.name.as_deref(),
        Message::Assistant(m) => m.name.as_deref(),
        Message::User(m) => m.name.as_deref(),
        _ => None,
    }
}

/// One text message, reasoning message or reasoning span.
#[derive(Debug)]
struct Item {
    owner: Owner,
    open: bool,
    /// Opened by a chunk and still assembling in its owner's lane.
    chunk: bool,
    /// Opening order, for [`ProducerState::closers`].
    seq: u64,
    /// Effective role (text messages only; `assistant` when absent).
    role: Option<TextMessageRole>,
    /// Fingerprint of `name` (text messages only).
    name: Option<u64>,
    /// The assistant message the reducer minted for a tool call's parent,
    /// before any text: its first text opener may set the name (TEXT-06).
    minted: bool,
}

/// One tool call.
#[derive(Debug)]
struct Call {
    owner: Owner,
    open: bool,
    chunk: bool,
    seq: u64,
    name: u64,
    parent: Option<MessageId>,
}

/// A subagent invocation as this run knows it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Sub {
    /// Events were attributed to it without an announcement (SUB-11).
    Attributed,
    /// Announced and not ended.
    Active { seq: u64 },
    /// Ended with `SUBAGENT_FINISHED` or `SUBAGENT_ERROR`.
    Ended,
}

/// An item opened in the run, in [`ProducerState::open_order`].
#[derive(Debug)]
enum Opened {
    Text(MessageId),
    Reasoning(MessageId),
    Span(MessageId),
    Call(ToolCallId),
    Subagent(SubagentRunId),
}

/// The kinds of chunk stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Text,
    Tool,
    Reasoning,
}

impl Kind {
    const fn chunk_type(self) -> &'static str {
        match self {
            Kind::Text => "TEXT_MESSAGE_CHUNK",
            Kind::Tool => "TOOL_CALL_CHUNK",
            Kind::Reasoning => "REASONING_MESSAGE_CHUNK",
        }
    }

    const fn id_field(self) -> &'static str {
        match self {
            Kind::Tool => "toolCallId",
            Kind::Text | Kind::Reasoning => "messageId",
        }
    }

    const fn rule(self) -> &'static str {
        match self {
            Kind::Text => "STRM-11",
            Kind::Tool => "STRM-14",
            Kind::Reasoning => "STRM-13",
        }
    }
}

/// The chunk stream pending in one lane.
#[derive(Debug, Clone)]
enum Lane {
    Text(MessageId),
    Tool(ToolCallId),
    Reasoning(MessageId),
}

impl Lane {
    const fn kind(&self) -> Kind {
        match self {
            Lane::Text(_) => Kind::Text,
            Lane::Tool(_) => Kind::Tool,
            Lane::Reasoning(_) => Kind::Reasoning,
        }
    }
}

/// A tool call of the input history.
#[derive(Debug)]
struct HistoryCall {
    /// Its name's fingerprint.
    name: u64,
    /// The assistant message holding it.
    parent: MessageId,
}

#[derive(Debug)]
struct ProducerState {
    input: Option<InputFacts>,
    phase: RunPhase,
    /// The stream's thread (the input's, or the first run's).
    thread: Option<ThreadId>,
    /// What the first `RUN_STARTED` declared (`Some(None)`: no version).
    version: Option<Option<ProtocolVersion>>,
    /// The run `RUN_STARTED` opened last.
    run: Option<RunId>,
    /// Every run id used on the stream (BASIC-14).
    runs: HashSet<RunId>,
    /// The requested run has started: nothing may follow it (RUNIN-31).
    requested_started: bool,
    /// Frontend tool names (fingerprints) for the current run.
    frontend: HashSet<u64>,
    seq: u64,

    /// Every message id used in the thread.
    messages: HashMap<MessageId, MessageKind>,
    /// Text and reasoning messages of the history and of snapshots.
    held: HashMap<MessageId, Held>,
    text: HashMap<MessageId, Item>,
    reasoning: HashMap<MessageId, Item>,
    spans: HashMap<MessageId, Item>,
    calls: HashMap<ToolCallId, Call>,
    /// Tool calls from the input history.
    history_calls: HashMap<ToolCallId, HistoryCall>,
    /// Tool messages of the input history: id → the call it answered.
    history_answers: HashMap<MessageId, ToolCallId>,
    /// Explicitly open items (text, reasoning, spans, calls, steps), for
    /// [`check_finish`](Self::check_finish).
    open_explicit: usize,
    /// Explicitly open items per subagent owner (STRM-26).
    open_owned: HashMap<SubagentRunId, usize>,
    /// Step name → owner → seq of each open step of that name, indexed by
    /// owner so many owners of one name cost O(1) each.
    steps: HashMap<String, HashMap<Owner, u64>>,
    /// Text, reasoning, spans, calls and invocations opened in the run, by
    /// `seq`. It may also hold ones since closed (the maps above keep ended
    /// items for the id rules); those are pruned before it grows, so
    /// [`closers`](Self::closers) costs O(open items), not O(items opened).
    open_order: Vec<(u64, Opened)>,
    /// Activity message → owner.
    activity: HashMap<MessageId, Owner>,
    subagents: HashMap<SubagentRunId, Sub>,
    active_subagents: usize,
    /// A subagent suspended in this run (INTR-02).
    suspended: bool,
    /// The parent agent's pending chunk stream.
    parent_lane: Option<Lane>,
    /// Subagent lanes with a pending chunk stream.
    lanes: HashMap<SubagentRunId, Lane>,
    /// Message (text or reasoning) id → key of the subagent lane assembling it.
    lane_messages: HashMap<MessageId, SubagentRunId>,
    /// Tool call id → key of the subagent lane assembling it.
    lane_calls: HashMap<ToolCallId, SubagentRunId>,
    /// Tool calls started in this run, in order.
    calls_order: Vec<ToolCallId>,
    /// Tool calls answered in this run.
    answered: HashSet<ToolCallId>,
    /// Lane probes made by `assembling` (test instrumentation).
    #[cfg(test)]
    lane_probes: std::sync::atomic::AtomicUsize,
    /// Entries `closers` examined (test instrumentation).
    #[cfg(test)]
    closer_probes: std::sync::atomic::AtomicUsize,
}

impl Default for ProducerState {
    fn default() -> Self {
        ProducerState {
            input: None,
            phase: RunPhase::Idle,
            thread: None,
            version: None,
            run: None,
            runs: HashSet::new(),
            requested_started: false,
            frontend: HashSet::new(),
            seq: 0,
            messages: HashMap::new(),
            held: HashMap::new(),
            text: HashMap::new(),
            reasoning: HashMap::new(),
            spans: HashMap::new(),
            calls: HashMap::new(),
            history_calls: HashMap::new(),
            history_answers: HashMap::new(),
            open_explicit: 0,
            open_owned: HashMap::new(),
            steps: HashMap::new(),
            open_order: Vec::new(),
            activity: HashMap::new(),
            subagents: HashMap::new(),
            active_subagents: 0,
            suspended: false,
            parent_lane: None,
            lanes: HashMap::new(),
            lane_messages: HashMap::new(),
            lane_calls: HashMap::new(),
            calls_order: Vec::new(),
            answered: HashSet::new(),
            #[cfg(test)]
            lane_probes: std::sync::atomic::AtomicUsize::new(0),
            #[cfg(test)]
            closer_probes: std::sync::atomic::AtomicUsize::new(0),
        }
    }
}

fn frontend_names(input: &RunAgentInput) -> HashSet<u64> {
    input
        .tools
        .iter()
        .flatten()
        .map(|tool| fingerprint(&tool.name))
        .collect()
}

fn role_text(role: TextMessageRole) -> &'static str {
    match role {
        TextMessageRole::Developer => "developer",
        TextMessageRole::System => "system",
        TextMessageRole::User => "user",
        _ => "assistant",
    }
}

/// Whether `text` is an RFC 6901 pointer (`^(/([^/~]|~[01])*)*$`): the
/// patch engine's own check, so the two never disagree.
fn is_pointer(text: &str) -> bool {
    crate::patch::is_valid_pointer(text)
}

/// Shape checks every event gets: values the types admit and the schema does
/// not (BASIC-09, ARCH-04).
fn check_values(event: &Event) -> Verdict {
    let event_type = event.event_type().as_str();
    if let Some(timestamp) = event.timestamp() {
        if !(-MAX_SAFE_INTEGER..=MAX_SAFE_INTEGER).contains(&timestamp) {
            return refuse(
                ProducerRule::InvalidValue,
                format!("{event_type}: timestamp {timestamp} is outside ±(2^53−1) (ARCH-04)"),
            );
        }
    }
    if matches!(event.raw_event(), Some(Value::Null)) {
        return refuse(
            ProducerRule::InvalidValue,
            format!("{event_type}: rawEvent is null; omit it instead (BASIC-09)"),
        );
    }
    if event
        .subagent_run_id()
        .is_some_and(|id| id.as_str().is_empty())
    {
        return refuse(
            ProducerRule::InvalidValue,
            format!("{event_type}: subagentRunId is empty (SUB-02, SUB-06)"),
        );
    }
    match event {
        Event::RunStarted(e) => check_run_started_values(e),
        Event::RunFinished(e) => {
            if matches!(e.result, Some(Value::Null)) {
                return refuse(
                    ProducerRule::InvalidValue,
                    "RUN_FINISHED: result is null; omit it instead (BASIC-09)",
                );
            }
            check_usage(event_type, e.usage.as_deref())
        }
        Event::RunError(e) => check_usage(event_type, e.usage.as_deref()),
        Event::SubagentFinished(e) if matches!(e.result, Some(Value::Null)) => refuse(
            ProducerRule::InvalidValue,
            "SUBAGENT_FINISHED: result is null; omit it instead (BASIC-09)",
        ),
        Event::StateDelta(e) => check_patch(event_type, &e.delta),
        Event::ActivityDelta(e) => check_patch(event_type, &e.patch),
        _ => Ok(()),
    }
}

fn check_run_started_values(e: &RunStarted) -> Verdict {
    // Absent is legal: a stream downgraded for a pre-versioning peer declares
    // nothing (VER-19, VER-28); the runner declares its own version otherwise
    // (VER-18).
    if let Some(declared) = &e.protocol_version {
        if ProtocolVersion::parse(declared).is_none() {
            return refuse(
                ProducerRule::InvalidValue,
                format!(
                    "RUN_STARTED: protocolVersion '{declared}' is not MAJOR.MINOR (VER-18)",
                    declared = shown(declared.as_ref())
                ),
            );
        }
    }
    if let Some(input) = &e.input {
        if matches!(input.state, Some(Value::Null)) {
            return refuse(
                ProducerRule::InvalidValue,
                "RUN_STARTED: input.state is null; omit it instead (BASIC-09)",
            );
        }
        if matches!(input.forwarded_props, Some(Value::Null)) {
            return refuse(
                ProducerRule::InvalidValue,
                "RUN_STARTED: input.forwardedProps is null; omit it instead (BASIC-09)",
            );
        }
        if let Some(tool) = input
            .tools
            .iter()
            .flatten()
            .find(|tool| matches!(tool.parameters, Some(Value::Null)))
        {
            return refuse(
                ProducerRule::InvalidValue,
                format!(
                    "RUN_STARTED: input tool '{}' has null parameters; omit them instead (BASIC-09)",
                    shown(&tool.name)
                ),
            );
        }
    }
    Ok(())
}

fn check_usage(event_type: &str, usage: Option<&[TokenUsage]>) -> Verdict {
    for (index, u) in usage.into_iter().flatten().enumerate() {
        let counts = [
            ("inputTokens", u.input_tokens),
            ("outputTokens", u.output_tokens),
            ("totalTokens", u.total_tokens),
            ("reasoningTokens", u.reasoning_tokens),
            ("cachedInputTokens", u.cached_input_tokens),
            ("cacheWriteInputTokens", u.cache_write_input_tokens),
        ];
        for (field, value) in counts {
            if value.is_some_and(|v| v > MAX_SAFE_INTEGER as u64) {
                return refuse(
                    ProducerRule::InvalidValue,
                    format!("{event_type}: usage[{index}].{field} exceeds 2^53−1 (ARCH-04)"),
                );
            }
        }
        if let (Some(input), Some(output), Some(total)) =
            (u.input_tokens, u.output_tokens, u.total_tokens)
        {
            if input.checked_add(output) != Some(total) {
                return refuse(
                    ProducerRule::InvalidValue,
                    format!(
                        "{event_type}: usage[{index}].totalTokens {total} is not inputTokens \
                         {input} + outputTokens {output} (LIFE-29)"
                    ),
                );
            }
        }
        if let Some(input) = u.input_tokens {
            let cached = u.cached_input_tokens.unwrap_or(0);
            let written = u.cache_write_input_tokens.unwrap_or(0);
            if cached
                .checked_add(written)
                .map_or(true, |parts| parts > input)
            {
                return refuse(
                    ProducerRule::InvalidValue,
                    format!(
                        "{event_type}: usage[{index}] cache counts exceed inputTokens {input}; \
                         they are parts of it, never additions (LIFE-26)"
                    ),
                );
            }
        }
        if let (Some(output), Some(reasoning)) = (u.output_tokens, u.reasoning_tokens) {
            if reasoning > output {
                return refuse(
                    ProducerRule::InvalidValue,
                    format!(
                        "{event_type}: usage[{index}].reasoningTokens {reasoning} exceeds \
                         outputTokens {output}; reasoning is part of the output (LIFE-26)"
                    ),
                );
            }
        }
    }
    Ok(())
}

fn check_patch(event_type: &str, ops: &[PatchOperation]) -> Verdict {
    for (index, op) in ops.iter().enumerate() {
        if !is_pointer(op.path.as_str()) {
            return refuse(
                ProducerRule::InvalidValue,
                format!("{event_type}: operation {index} path is not an RFC 6901 pointer"),
            );
        }
        match op.op {
            PatchOp::Add | PatchOp::Replace | PatchOp::Test if op.value.is_none() => {
                return refuse(
                    ProducerRule::InvalidValue,
                    format!("{event_type}: operation {index} has no value"),
                );
            }
            PatchOp::Move | PatchOp::Copy => match &op.from {
                None => {
                    return refuse(
                        ProducerRule::InvalidValue,
                        format!("{event_type}: operation {index} has no from"),
                    );
                }
                Some(from) if !is_pointer(from.as_str()) => {
                    return refuse(
                        ProducerRule::InvalidValue,
                        format!("{event_type}: operation {index} from is not an RFC 6901 pointer"),
                    );
                }
                Some(_) => {}
            },
            _ => {}
        }
    }
    Ok(())
}

/// ACT-15: no operation may leave the activity content anything but an
/// object. Only an operation on the root can change the content's type; a
/// `move`/`copy` onto the root cannot be judged without the content, so it is
/// refused (send `ACTIVITY_SNAPSHOT` instead).
fn check_activity_root(ops: &[PatchOperation]) -> Verdict {
    for (index, op) in ops.iter().enumerate() {
        if !op.path.as_str().is_empty() {
            continue;
        }
        let keeps_object = match op.op {
            PatchOp::Add | PatchOp::Replace => matches!(op.value, Some(Value::Object(_))),
            PatchOp::Test => true,
            _ => false,
        };
        if !keeps_object {
            return refuse(
                ProducerRule::InvalidValue,
                format!(
                    "ACTIVITY_DELTA: operation {index} would make the activity content something \
                     other than an object; restate it with ACTIVITY_SNAPSHOT (ACT-15)"
                ),
            );
        }
    }
    Ok(())
}

impl ProducerState {
    fn next_seq(&mut self) -> u64 {
        self.seq += 1;
        self.seq
    }

    /// The `seq` of a newly opened item, recorded in `open_order`.
    fn opened(&mut self, item: Opened) -> u64 {
        let seq = self.next_seq();
        if self.open_order.len() == self.open_order.capacity() {
            let mut order = std::mem::take(&mut self.open_order);
            order.retain(|(seq, item)| self.still_open(*seq, item));
            // Room for at least as many again before the next pruning, so
            // pruning is amortized O(1) per opened item.
            order.reserve(order.len().max(4));
            self.open_order = order;
        }
        self.open_order.push((seq, item));
        seq
    }

    /// Whether the `open_order` entry `(seq, item)` is still open.
    fn still_open(&self, seq: u64, item: &Opened) -> bool {
        match item {
            Opened::Text(id) => self.text.get(id).is_some_and(|i| i.open && i.seq == seq),
            Opened::Reasoning(id) => self
                .reasoning
                .get(id)
                .is_some_and(|i| i.open && i.seq == seq),
            Opened::Span(id) => self.spans.get(id).is_some_and(|i| i.open && i.seq == seq),
            Opened::Call(id) => self.calls.get(id).is_some_and(|c| c.open && c.seq == seq),
            Opened::Subagent(id) => {
                matches!(self.subagents.get(id), Some(Sub::Active { seq: s }) if *s == seq)
            }
        }
    }

    fn register_history(&mut self, messages: &[Message]) {
        for message in messages {
            self.messages
                .entry(message.id().clone())
                .or_insert(MessageKind::History);
            if let Some(held) = Held::of(message) {
                self.held.entry(message.id().clone()).or_insert(held);
            }
            if let Message::Activity(activity) = message {
                self.activity
                    .entry(activity.id.clone())
                    .or_insert_with(|| activity.subagent_run_id.clone());
            }
            if let Message::Tool(tool) = message {
                self.history_answers
                    .entry(tool.id.clone())
                    .or_insert_with(|| tool.tool_call_id.clone());
            }
            for call in message.tool_calls() {
                self.history_calls
                    .entry(call.id.clone())
                    .or_insert_with(|| HistoryCall {
                        name: fingerprint(&call.function.name),
                        parent: message.id().clone(),
                    });
            }
        }
    }

    fn check(&mut self, event: &Event) -> Verdict {
        self.gate(event)?;
        check_values(event)?;
        match event {
            Event::RunStarted(e) => self.run_started(e),
            Event::RunFinished(e) => self.run_finished(e),
            Event::RunError(_) => {
                self.end_run(RunPhase::Failed);
                Ok(())
            }
            _ => self.in_run(event),
        }
    }

    /// The lifecycle gate (LIFE-01, LIFE-02, LIFE-37, LIFE-41, LIFE-44).
    fn gate(&self, event: &Event) -> Verdict {
        let event_type = event.event_type().as_str();
        match (self.phase, event) {
            (RunPhase::Running, Event::RunStarted(_)) => refuse(
                ProducerRule::NestedRun,
                "RUN_STARTED while a run is active; end it with RUN_FINISHED or RUN_ERROR first \
                 (LIFE-44)",
            ),
            (RunPhase::Running, _) => Ok(()),
            (RunPhase::Idle, Event::RunStarted(_) | Event::RunError(_)) => Ok(()),
            (RunPhase::Idle, _) => refuse(
                ProducerRule::FirstEvent,
                format!(
                    "{event_type} before RUN_STARTED; a stream begins with RUN_STARTED or \
                     RUN_ERROR (LIFE-01, LIFE-02)"
                ),
            ),
            (RunPhase::Finished, Event::RunStarted(_) | Event::RunError(_)) => Ok(()),
            (RunPhase::Failed, Event::RunStarted(_)) => Ok(()),
            (RunPhase::Finished, _) => refuse(
                ProducerRule::AfterClose,
                format!(
                    "{event_type} after RUN_FINISHED; only RUN_STARTED or a late RUN_ERROR may \
                     follow (LIFE-37)"
                ),
            ),
            (_, _) => refuse(
                ProducerRule::AfterClose,
                format!("{event_type} after RUN_ERROR; only RUN_STARTED may follow (LIFE-41)"),
            ),
        }
    }

    fn run_started(&mut self, e: &RunStarted) -> Verdict {
        // `check_values` guarantees a parseable version when one is declared.
        let declared = e
            .protocol_version
            .as_deref()
            .and_then(ProtocolVersion::parse);
        // Under `for_input` every run before the requested one is replay,
        // which declares its own version, and RUNIN-31 leaves the requested
        // run the only live one; only a bare stream is held to one version.
        if let (Some(first), None) = (self.version, &self.input) {
            if first != declared {
                let text = |v: Option<ProtocolVersion>| match v {
                    Some(v) => format!("{}.{}", v.major, v.minor),
                    None => "no version".to_owned(),
                };
                return refuse(
                    ProducerRule::InvalidValue,
                    format!(
                        "RUN_STARTED declares {} after {} on the same stream; a live producer \
                         declares one version (VER-28)",
                        text(declared),
                        text(first)
                    ),
                );
            }
        }
        if let Some(thread) = &self.thread {
            if *thread != e.thread_id {
                return refuse(
                    ProducerRule::Identity,
                    format!(
                        "RUN_STARTED threadId '{}' is not the stream's thread '{thread}' \
                         (RUNIN-04)",
                        shown(e.thread_id.as_str()),
                        thread = shown(thread.as_ref())
                    ),
                );
            }
        }
        if self.runs.contains(&e.run_id) {
            return refuse(
                ProducerRule::Identity,
                format!(
                    "RUN_STARTED reuses runId '{}' on this thread (BASIC-14)",
                    shown(e.run_id.as_str())
                ),
            );
        }
        if self.requested_started {
            return refuse(
                ProducerRule::Identity,
                format!(
                    "RUN_STARTED '{}' after the requested run; the requested run is the \
                     exchange's last (RUNIN-31)",
                    shown(e.run_id.as_str())
                ),
            );
        }
        if let Some(echo) = &e.input {
            if echo.thread_id != e.thread_id || echo.run_id != e.run_id {
                return refuse(
                    ProducerRule::Identity,
                    format!(
                        "RUN_STARTED '{}' echoes the input of run '{}' on thread '{}' (RUNIN-06)",
                        shown(e.run_id.as_str()),
                        shown(echo.run_id.as_str()),
                        shown(echo.thread_id.as_str())
                    ),
                );
            }
        }

        let requested = self
            .input
            .as_ref()
            .is_some_and(|input| input.run_id == e.run_id);
        self.end_run(self.phase);
        self.phase = RunPhase::Running;
        self.thread.get_or_insert_with(|| e.thread_id.clone());
        if self.version.is_none() {
            self.version = Some(declared);
        }
        self.run = Some(e.run_id.clone());
        self.runs.insert(e.run_id.clone());
        self.frontend.clear();
        if requested {
            self.requested_started = true;
            if let Some(input) = &self.input {
                self.frontend.extend(input.frontend.iter().copied());
            }
        }
        if let Some(echo) = &e.input {
            self.frontend.extend(frontend_names(echo));
            self.register_history(&echo.messages);
        }
        Ok(())
    }

    fn run_finished(&mut self, e: &crate::lifecycle::RunFinished) -> Verdict {
        if self.run.as_ref() != Some(&e.run_id) || self.thread.as_ref() != Some(&e.thread_id) {
            return refuse(
                ProducerRule::Identity,
                format!(
                    "RUN_FINISHED names run '{}' on thread '{}', but RUN_STARTED opened run '{}' \
                     on thread '{}' (RUNIN-04, RUNIN-05)",
                    shown(e.run_id.as_str()),
                    shown(e.thread_id.as_str()),
                    shown(self.run.as_ref().map_or("", |r| r.as_str())),
                    shown(self.thread.as_ref().map_or("", |t| t.as_str())),
                ),
            );
        }
        self.check_finish()?;
        self.check_outcome(e)?;
        self.end_run(RunPhase::Finished);
        Ok(())
    }

    /// Nothing explicitly opened may be open at `RUN_FINISHED` (STRM-03,
    /// REAS-03, LIFE-12, LIFE-51, SUB-10, SUB-33). Chunk streams are ended by
    /// the terminal itself.
    fn check_finish(&self) -> Verdict {
        if self.open_explicit == 0 && self.active_subagents == 0 {
            return Ok(());
        }
        let mut open: Vec<String> = Vec::new();
        let ids = |noun: &str, map: &HashMap<MessageId, Item>, out: &mut Vec<String>| {
            for (id, item) in map {
                if item.open && !item.chunk {
                    out.push(format!("{noun} '{id}'", id = shown(id.as_ref())));
                }
            }
        };
        ids("text message", &self.text, &mut open);
        ids("reasoning message", &self.reasoning, &mut open);
        ids("reasoning span", &self.spans, &mut open);
        for (id, call) in &self.calls {
            if call.open && !call.chunk {
                open.push(format!("tool call '{id}'", id = shown(id.as_ref())));
            }
        }
        for (name, holders) in &self.steps {
            for owner in holders.keys() {
                open.push(format!(
                    "step '{name}' of {}",
                    owner_text(owner),
                    name = shown(name.as_ref())
                ));
            }
        }
        for (id, sub) in &self.subagents {
            if matches!(sub, Sub::Active { .. }) {
                open.push(format!(
                    "subagent invocation '{id}'",
                    id = shown(id.as_ref())
                ));
            }
        }
        open.sort_unstable();
        let more = open.len().saturating_sub(MAX_LISTED_IDS);
        open.truncate(MAX_LISTED_IDS);
        let mut listed = open.join(", ");
        if more > 0 {
            listed.push_str(&format!(", and {more} more"));
        }
        refuse(
            ProducerRule::OpenAtFinish,
            format!(
                "RUN_FINISHED with {listed} still open; close everything the run opened first \
                 (STRM-03, REAS-03, LIFE-12, LIFE-51, SUB-10, SUB-33)"
            ),
        )
    }

    fn check_outcome(&self, e: &crate::lifecycle::RunFinished) -> Verdict {
        match &e.outcome {
            None | Some(RunFinishedOutcome::Success { .. }) if self.suspended => refuse(
                ProducerRule::Outcome,
                "RUN_FINISHED reports success while a subagent of this run is suspended awaiting \
                 input; end with the interrupt outcome (INTR-02)",
            ),
            Some(RunFinishedOutcome::Success {
                pending_tool_call_ids: Some(pending),
            }) if !pending.is_empty() => {
                // An empty list means the producer named none (schema).
                let unanswered = self
                    .calls_order
                    .iter()
                    .filter(|id| !self.answered.contains(*id));
                if !pending.iter().eq(unanswered) {
                    let expected = quote_list(
                        self.calls_order
                            .iter()
                            .filter(|id| !self.answered.contains(*id)),
                    );
                    let sent = quote_list(pending);
                    return refuse(
                        ProducerRule::Outcome,
                        format!(
                            "pendingToolCallIds [{sent}] must be exactly the unanswered calls of \
                             this run in the order made, [{expected}] (TOOL-29, TOOL-31)"
                        ),
                    );
                }
                Ok(())
            }
            Some(RunFinishedOutcome::Interrupt { interrupts }) => {
                if interrupts.is_empty() {
                    return refuse(
                        ProducerRule::Outcome,
                        "RUN_FINISHED interrupt outcome with no interrupts (INTR-01)",
                    );
                }
                let mut seen: HashSet<&InterruptId> = HashSet::with_capacity(interrupts.len());
                for interrupt in interrupts {
                    if !seen.insert(&interrupt.id) {
                        return refuse(
                            ProducerRule::DuplicateInterrupt,
                            format!(
                                "interrupt id '{}' appears twice in the run (INTR-04)",
                                shown(interrupt.id.as_str())
                            ),
                        );
                    }
                    if let Some(call) = &interrupt.tool_call_id {
                        if self.is_pending_frontend(call) {
                            return refuse(
                                ProducerRule::Outcome,
                                format!(
                                    "interrupt '{}' reports the frontend tool call '{call}' as an \
                                     interrupt; a frontend-tool stop finishes as success \
                                     (TOOL-26)",
                                    shown(interrupt.id.as_str()),
                                    call = shown(call.as_ref())
                                ),
                            );
                        }
                    }
                }
                Ok(())
            }
            Some(RunFinishedOutcome::Cancelled) if e.result.is_some() => refuse(
                ProducerRule::Outcome,
                "RUN_FINISHED cancelled outcome carries a result; a stopped run has none \
                 (LIFE-14)",
            ),
            _ => Ok(()),
        }
    }

    fn is_pending_frontend(&self, id: &ToolCallId) -> bool {
        !self.answered.contains(id)
            && self
                .calls
                .get(id)
                .is_some_and(|call| self.frontend.contains(&call.name))
    }

    /// Ends the run: everything run-scoped is forgotten. Items are tracked per
    /// run; only message ids outlive the run (a later run's opener for an id
    /// reopens it when the kinds match, else BASIC-15 refuses it through
    /// `messages`). A table is cleared for reuse, or dropped when far larger
    /// than the run that just used it, so this is amortized O(that run): the
    /// one end of run that drops a large table costs O(its capacity), once.
    fn end_run(&mut self, phase: RunPhase) {
        use crate::pipeline::{oversized, reset_map, reset_set};
        self.phase = phase;
        reset_map(&mut self.text);
        reset_map(&mut self.reasoning);
        reset_map(&mut self.spans);
        reset_map(&mut self.calls);
        self.open_explicit = 0;
        reset_map(&mut self.open_owned);
        reset_map(&mut self.steps);
        if oversized(self.open_order.capacity(), self.open_order.len()) {
            self.open_order = Vec::new();
        } else {
            self.open_order.clear();
        }
        reset_map(&mut self.subagents);
        self.active_subagents = 0;
        self.suspended = false;
        self.parent_lane = None;
        reset_map(&mut self.lanes);
        reset_map(&mut self.lane_messages);
        reset_map(&mut self.lane_calls);
        if oversized(self.calls_order.capacity(), self.calls_order.len()) {
            self.calls_order = Vec::new();
        } else {
            self.calls_order.clear();
        }
        reset_set(&mut self.answered);
    }

    /// The lane `owner`'s pending chunk stream.
    fn lane(&self, owner: &Owner) -> Option<&Lane> {
        match owner {
            None => self.parent_lane.as_ref(),
            Some(id) => self.lanes.get(id),
        }
    }

    /// Ends the chunk stream pending in `owner`'s lane, if any.
    fn close_lane(&mut self, owner: &Owner) {
        let lane = match owner {
            None => self.parent_lane.take(),
            Some(id) => {
                let lane = self.lanes.remove(id);
                if let Some(lane) = &lane {
                    self.unindex(lane);
                }
                lane
            }
        };
        match lane {
            Some(Lane::Text(id)) => {
                if let Some(item) = self.text.get_mut(&id) {
                    item.open = false;
                    item.chunk = false;
                }
            }
            Some(Lane::Reasoning(id)) => {
                if let Some(item) = self.reasoning.get_mut(&id) {
                    item.open = false;
                    item.chunk = false;
                }
            }
            Some(Lane::Tool(id)) => {
                if let Some(call) = self.calls.get_mut(&id) {
                    call.open = false;
                    call.chunk = false;
                }
            }
            None => {}
        }
    }

    fn close_all_lanes(&mut self) {
        self.close_lane(&None);
        let owners: Vec<SubagentRunId> = self.lanes.keys().cloned().collect();
        for owner in owners {
            self.close_lane(&Some(owner));
        }
    }

    fn opened_explicit(&mut self, owner: &Owner) {
        self.open_explicit += 1;
        if let Some(id) = owner {
            *self.open_owned.entry(id.clone()).or_insert(0) += 1;
        }
    }

    fn closed_explicit(&mut self, owner: &Owner) {
        self.open_explicit = self.open_explicit.saturating_sub(1);
        if let Some(id) = owner {
            if let Some(count) = self.open_owned.get_mut(id) {
                *count = count.saturating_sub(1);
            }
        }
    }

    /// Notes attribution to `owner` (SUB-11) and refuses it for nothing:
    /// a tag after the invocation ended is provenance, not a violation.
    fn attribute(&mut self, owner: &Owner) {
        if let Some(id) = owner {
            if !self.subagents.contains_key(id) {
                self.subagents.insert(id.clone(), Sub::Attributed);
            }
        }
    }

    fn in_run(&mut self, event: &Event) -> Verdict {
        let tag: Owner = event.subagent_run_id().cloned();
        match event {
            Event::TextMessageStart(e) => self.text_start(e, &tag),
            Event::TextMessageContent(e) => {
                self.explicit_continue(Family::Text, &e.message_id, &tag, false)
            }
            Event::TextMessageEnd(e) => {
                self.explicit_continue(Family::Text, &e.message_id, &tag, true)
            }
            Event::ReasoningMessageStart(e) => self.reasoning_start(&e.message_id, &tag),
            Event::ReasoningMessageContent(e) => {
                self.explicit_continue(Family::Reasoning, &e.message_id, &tag, false)
            }
            Event::ReasoningMessageEnd(e) => {
                self.explicit_continue(Family::Reasoning, &e.message_id, &tag, true)
            }
            Event::ReasoningStart(e) => self.span_start(&e.message_id, &tag),
            Event::ReasoningEnd(e) => {
                self.explicit_continue(Family::Span, &e.message_id, &tag, true)
            }
            Event::ToolCallStart(e) => self.tool_start(
                &e.tool_call_id,
                &e.tool_call_name,
                e.parent_message_id.as_ref(),
                &tag,
            ),
            Event::ToolCallArgs(e) => self.tool_continue(&e.tool_call_id, &tag, false),
            Event::ToolCallEnd(e) => self.tool_continue(&e.tool_call_id, &tag, true),
            Event::ToolCallResult(e) => self.tool_result(&e.message_id, &e.tool_call_id, &tag),
            Event::TextMessageChunk(e) => self.text_chunk(e, &tag),
            Event::ToolCallChunk(e) => self.tool_chunk(e, &tag),
            Event::ReasoningMessageChunk(e) => self.reasoning_chunk(e.message_id.as_ref(), &tag),
            Event::StepStarted(e) => self.step_started(&e.step_name, &tag),
            Event::StepFinished(e) => self.step_finished(&e.step_name, &tag),
            Event::SubagentStarted(e) => self.subagent_started(e),
            Event::SubagentFinished(e) => {
                let suspended =
                    matches!(e.outcome, Some(SubagentFinishedOutcome::Suspended { .. }));
                self.subagent_ended(&e.subagent_run_id, "SUBAGENT_FINISHED", suspended)
            }
            Event::SubagentError(e) => {
                self.subagent_ended(&e.subagent_run_id, "SUBAGENT_ERROR", false)
            }
            Event::ActivitySnapshot(e) => self.activity_snapshot(e, &tag),
            Event::ActivityDelta(e) => self.activity_delta(e, &tag),
            Event::MessagesSnapshot(e) => self.messages_snapshot(&e.messages),
            // Standalone: open and close nothing, end no chunk stream
            // (STRM-06, REAS-19, PASS-01).
            Event::Raw(_) | Event::ReasoningEncryptedValue(_) => {
                self.attribute(&tag);
                Ok(())
            }
            // STATE_*, CUSTOM: standalone, but end the lane's chunk stream.
            _ => {
                self.attribute(&tag);
                self.close_lane(&tag);
                Ok(())
            }
        }
    }

    /// The owner of the subagent lane assembling `id` of `kind`, if any.
    fn assembling(&self, kind: Kind, id: &str) -> Option<Owner> {
        let names = |lane: &Lane| {
            #[cfg(test)]
            self.lane_probes
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            match lane {
                Lane::Text(m) if kind == Kind::Text => m.as_str() == id,
                Lane::Reasoning(m) if kind == Kind::Reasoning => m.as_str() == id,
                Lane::Tool(c) if kind == Kind::Tool => c.as_str() == id,
                _ => false,
            }
        };
        if self.parent_lane.as_ref().is_some_and(names) {
            return Some(None);
        }
        let owner = if kind == Kind::Tool {
            self.lane_calls.get(id)
        } else {
            self.lane_messages.get(id)
        }?;
        self.lanes
            .get(owner)
            .filter(|lane| names(lane))
            .map(|_| Some(owner.clone()))
    }

    /// BASIC-15: a new message may not take an id the thread already uses.
    fn check_new_message(&self, id: &MessageId, event_type: &str) -> Verdict {
        match self.messages.get(id) {
            Some(kind) => refuse(
                ProducerRule::DuplicateMessageId,
                format!(
                    "{event_type} opens a new message with messageId '{id}', already used by {} \
                     in this thread (BASIC-15)",
                    kind.noun(),
                    id = shown(id.as_ref())
                ),
            ),
            None => Ok(()),
        }
    }

    /// An opener for `id`, which this run does not track: a reopening of a
    /// message the thread holds when it is one of the opener's kind (a text
    /// message for `role: Some`, else a reasoning message), else a new
    /// message (BASIC-15). A message from an earlier run reopens freely (its
    /// tracking has reset); one from the history or a snapshot agrees with
    /// its owner and role (TEXT-06, SUB-18).
    fn check_untracked_opener(
        &self,
        id: &MessageId,
        tag: &Owner,
        role: Option<Option<TextMessageRole>>,
        event_type: &str,
    ) -> Verdict {
        let text = role.is_some();
        match self.messages.get(id) {
            Some(MessageKind::Text) if text => Ok(()),
            Some(MessageKind::Reasoning) if !text => Ok(()),
            Some(MessageKind::History | MessageKind::Snapshot) => match self.held.get(id) {
                Some(held) if held.role.is_some() == text => {
                    if held.owner != *tag {
                        return refuse(
                            ProducerRule::OwnerMismatch,
                            format!(
                                "{event_type} reopens message '{id}' under {}, but it belongs to \
                                 {} (TEXT-06, SUB-18)",
                                owner_text(tag),
                                owner_text(&held.owner),
                                id = shown(id.as_ref())
                            ),
                        );
                    }
                    if let (Some(Some(role)), Some(had)) = (role, held.role) {
                        if role != had {
                            return refuse(
                                ProducerRule::OwnerMismatch,
                                format!(
                                    "{event_type} reopens message '{id}' as role '{}', but it is \
                                     '{}' (TEXT-06)",
                                    role_text(role),
                                    role_text(had),
                                    id = shown(id.as_ref())
                                ),
                            );
                        }
                    }
                    Ok(())
                }
                _ => self.check_new_message(id, event_type),
            },
            _ => self.check_new_message(id, event_type),
        }
    }

    fn text_start(&mut self, e: &crate::text::TextMessageStart, tag: &Owner) -> Verdict {
        let role = e.role.unwrap_or(TextMessageRole::Assistant);
        let name = e.name.as_deref().map(fingerprint);
        self.check_text_opener(&e.message_id, tag, Some(role), name, "TEXT_MESSAGE_START")?;
        self.attribute(tag);
        self.close_lane(tag);
        let seq = self.opened(Opened::Text(e.message_id.clone()));
        self.text.insert(
            e.message_id.clone(),
            Item {
                owner: tag.clone(),
                open: true,
                chunk: false,
                seq,
                role: Some(role),
                name,
                minted: false,
            },
        );
        self.messages
            .entry(e.message_id.clone())
            .or_insert(MessageKind::Text);
        self.opened_explicit(tag);
        Ok(())
    }

    /// Checks a text opener (explicit or chunk): not open, not assembling,
    /// a reopening agrees (TEXT-06), a new id is unused (BASIC-15).
    /// `role: None` on a reopening chunk means "not restated".
    fn check_text_opener(
        &self,
        id: &MessageId,
        tag: &Owner,
        role: Option<TextMessageRole>,
        name: Option<u64>,
        event_type: &str,
    ) -> Verdict {
        match self.text.get(id) {
            Some(item) if item.open && item.chunk => refuse(
                ProducerRule::ChunkDiscipline,
                format!(
                    "{event_type} for message '{id}', which is assembling from chunks; a \
                     chunk-opened message continues in chunk form (STRM-09)",
                    id = shown(id.as_ref())
                ),
            ),
            Some(item) if item.open && event_type == "TEXT_MESSAGE_CHUNK" => refuse(
                ProducerRule::ChunkDiscipline,
                format!(
                    "{event_type} for message '{id}', which TEXT_MESSAGE_START opened; an \
                     explicitly opened message continues with explicit events (STRM-09)",
                    id = shown(id.as_ref())
                ),
            ),
            Some(item) if item.open => refuse(
                ProducerRule::AlreadyOpen,
                format!(
                    "{event_type} for message '{id}', which is already open (STRM-01)",
                    id = shown(id.as_ref())
                ),
            ),
            Some(item) => {
                if item.owner != *tag {
                    return refuse(
                        ProducerRule::OwnerMismatch,
                        format!(
                            "{event_type} reopens message '{id}' under {}, but it belongs to {} \
                             (TEXT-06, SUB-18)",
                            owner_text(tag),
                            owner_text(&item.owner),
                            id = shown(id.as_ref())
                        ),
                    );
                }
                if let (Some(role), Some(had)) = (role, item.role) {
                    if role != had {
                        return refuse(
                            ProducerRule::OwnerMismatch,
                            format!(
                                "{event_type} reopens message '{id}' as role '{}', but it is \
                                 '{}' (TEXT-06)",
                                role_text(role),
                                role_text(had),
                                id = shown(id.as_ref())
                            ),
                        );
                    }
                }
                // A minted carrier takes its first opener's name, as the
                // reducer fills it.
                if role.is_some() && !item.minted && name != item.name {
                    return refuse(
                        ProducerRule::OwnerMismatch,
                        format!(
                            "{event_type} reopens message '{id}' with a different name (TEXT-06)",
                            id = shown(id.as_ref())
                        ),
                    );
                }
                Ok(())
            }
            None => self.check_untracked_opener(id, tag, Some(role), event_type),
        }
    }

    fn reasoning_start(&mut self, id: &MessageId, tag: &Owner) -> Verdict {
        self.check_reasoning_opener(id, tag, "REASONING_MESSAGE_START")?;
        self.attribute(tag);
        self.close_lane(tag);
        self.open_reasoning(id, tag, false);
        self.opened_explicit(tag);
        Ok(())
    }

    fn check_reasoning_opener(&self, id: &MessageId, tag: &Owner, event_type: &str) -> Verdict {
        match self.reasoning.get(id) {
            Some(item) if item.open && item.chunk => refuse(
                ProducerRule::ChunkDiscipline,
                format!(
                    "{event_type} for reasoning message '{id}', which is assembling from chunks \
                     (STRM-09)",
                    id = shown(id.as_ref())
                ),
            ),
            Some(item) if item.open && event_type == "REASONING_MESSAGE_CHUNK" => refuse(
                ProducerRule::ChunkDiscipline,
                format!(
                    "{event_type} for reasoning message '{id}', which REASONING_MESSAGE_START \
                     opened; it continues with explicit events (STRM-09)",
                    id = shown(id.as_ref())
                ),
            ),
            Some(item) if item.open => refuse(
                ProducerRule::AlreadyOpen,
                format!(
                    "{event_type} for reasoning message '{id}', which is already open (STRM-01)",
                    id = shown(id.as_ref())
                ),
            ),
            Some(item) if item.owner != *tag => refuse(
                ProducerRule::OwnerMismatch,
                format!(
                    "{event_type} reopens reasoning message '{id}' under {}, but it belongs to {} \
                     (SUB-21)",
                    owner_text(tag),
                    owner_text(&item.owner),
                    id = shown(id.as_ref())
                ),
            ),
            Some(_) => Ok(()),
            None => self.check_untracked_opener(id, tag, None, event_type),
        }
    }

    fn open_reasoning(&mut self, id: &MessageId, owner: &Owner, chunk: bool) {
        let seq = self.opened(Opened::Reasoning(id.clone()));
        self.reasoning.insert(
            id.clone(),
            Item {
                owner: owner.clone(),
                open: true,
                chunk,
                seq,
                role: None,
                name: None,
                minted: false,
            },
        );
        self.messages
            .entry(id.clone())
            .or_insert(MessageKind::Reasoning);
    }

    fn span_start(&mut self, id: &MessageId, tag: &Owner) -> Verdict {
        if self.spans.get(id).is_some_and(|span| span.open) {
            return refuse(
                ProducerRule::AlreadyOpen,
                format!(
                    "REASONING_START for span '{id}', which is already open (REAS-02)",
                    id = shown(id.as_ref())
                ),
            );
        }
        self.attribute(tag);
        self.close_lane(tag);
        let seq = self.opened(Opened::Span(id.clone()));
        self.spans.insert(
            id.clone(),
            Item {
                owner: tag.clone(),
                open: true,
                chunk: false,
                seq,
                role: None,
                name: None,
                minted: false,
            },
        );
        self.opened_explicit(tag);
        Ok(())
    }

    /// Content or end for an explicitly opened text message, reasoning
    /// message or span (STRM-02, STRM-09, REAS-04, SUB-18..SUB-21).
    fn explicit_continue(
        &mut self,
        family: Family,
        id: &MessageId,
        tag: &Owner,
        end: bool,
    ) -> Verdict {
        let event_type = family.event_type(end);
        let map = match family {
            Family::Text => &self.text,
            Family::Reasoning => &self.reasoning,
            Family::Span => &self.spans,
        };
        let owner = match map.get(id) {
            Some(item) if item.open && item.chunk => {
                return refuse(
                    ProducerRule::ChunkDiscipline,
                    format!(
                        "{event_type} for {} '{id}', which is assembling from chunks; its end is \
                         implied and its content is sent as chunks (STRM-09)",
                        family.noun(),
                        id = shown(id.as_ref())
                    ),
                )
            }
            Some(item) if item.open => item.owner.clone(),
            _ => {
                return refuse(
                    ProducerRule::NotOpen,
                    format!(
                        "{event_type} for {} '{id}', which is not open ({})",
                        family.noun(),
                        family.not_open_rule(),
                        id = shown(id.as_ref())
                    ),
                )
            }
        };
        if tag.is_some() && *tag != owner {
            return refuse(
                ProducerRule::OwnerMismatch,
                format!(
                    "{event_type} for {} '{id}' is attributed to {}, but its opener belongs to {} \
                     (SUB-18, SUB-19, SUB-21)",
                    family.noun(),
                    owner_text(tag),
                    owner_text(&owner),
                    id = shown(id.as_ref())
                ),
            );
        }
        self.attribute(tag);
        // An explicit event ends the chunk stream in its own lane: its tag's,
        // as the consumer's chunk expander reckons it.
        self.close_lane(tag);
        if end {
            let map = match family {
                Family::Text => &mut self.text,
                Family::Reasoning => &mut self.reasoning,
                Family::Span => &mut self.spans,
            };
            if let Some(item) = map.get_mut(id) {
                item.open = false;
            }
            self.closed_explicit(&owner);
        }
        Ok(())
    }

    fn tool_start(
        &mut self,
        id: &ToolCallId,
        name: &str,
        parent: Option<&MessageId>,
        tag: &Owner,
    ) -> Verdict {
        let owner = self.check_tool_opener(id, Some(name), parent, tag, "TOOL_CALL_START")?;
        self.attribute(&owner);
        self.close_lane(tag);
        self.open_call(id, fingerprint(name), parent, &owner, false);
        self.opened_explicit(&owner);
        Ok(())
    }

    /// Checks a tool-call opener; returns the call's owner (its tag, or its
    /// parent message's owner when untagged).
    fn check_tool_opener(
        &self,
        id: &ToolCallId,
        name: Option<&str>,
        parent: Option<&MessageId>,
        tag: &Owner,
        event_type: &str,
    ) -> std::result::Result<Owner, ProducerViolation> {
        let parent_owner = parent.and_then(|p| {
            self.text
                .get(p)
                .map(|item| item.owner.clone())
                .or_else(|| self.reasoning.get(p).map(|item| item.owner.clone()))
                .or_else(|| self.held.get(p).map(|held| held.owner.clone()))
        });
        // A call inherits the attribution of the message holding it (SUB-26,
        // TOOL-02): an untagged call takes its parent message's owner, and only
        // a tag naming another owner disagrees.
        if let Some(parent_owner) = &parent_owner {
            if tag.is_some() && parent_owner != tag {
                return Err(ProducerViolation::new(
                    ProducerRule::OwnerMismatch,
                    format!(
                        "{event_type} '{id}' is attributed to {}, but its parent message '{}' \
                         belongs to {}; a call carries its message's attribution (TOOL-02, \
                         SUB-26)",
                        owner_text(tag),
                        shown(parent.map_or("", |p| p.as_str())),
                        owner_text(parent_owner),
                        id = shown(id.as_ref())
                    ),
                ));
            }
        }
        let owner = match (tag, parent_owner) {
            (None, Some(parent_owner)) => parent_owner,
            _ => tag.clone(),
        };
        match self.calls.get(id) {
            Some(call) if call.open && call.chunk => Err(ProducerViolation::new(
                ProducerRule::ChunkDiscipline,
                format!(
                    "{event_type} for tool call '{id}', which is assembling from chunks (STRM-09)",
                    id = shown(id.as_ref())
                ),
            )),
            Some(call) if call.open && event_type == "TOOL_CALL_CHUNK" => {
                Err(ProducerViolation::new(
                    ProducerRule::ChunkDiscipline,
                    format!(
                        "{event_type} for tool call '{id}', which TOOL_CALL_START opened; it \
                         continues with explicit events (STRM-09)",
                        id = shown(id.as_ref())
                    ),
                ))
            }
            Some(call) if call.open => Err(ProducerViolation::new(
                ProducerRule::AlreadyOpen,
                format!(
                    "{event_type} for tool call '{id}', which is already open (STRM-01)",
                    id = shown(id.as_ref())
                ),
            )),
            Some(call) => {
                let disagrees = if call.owner != owner {
                    Some("owner")
                } else if name.is_some_and(|n| fingerprint(n) != call.name) {
                    Some("toolCallName")
                } else if parent.is_some() && parent != call.parent.as_ref() {
                    Some("parentMessageId")
                } else {
                    None
                };
                match disagrees {
                    Some(field) => Err(ProducerViolation::new(
                        ProducerRule::OwnerMismatch,
                        format!(
                            "{event_type} reopens tool call '{id}' with a different {field} \
                             (TOOL-07)",
                            id = shown(id.as_ref())
                        ),
                    )),
                    None => Ok(owner),
                }
            }
            // A call of the input history reopens in place, under its own
            // name, parent and owner (the holding message's) (TOOL-07).
            None => {
                let Some(had) = self.history_calls.get(id) else {
                    return Ok(owner);
                };
                let holder_owner = self.held.get(&had.parent).and_then(|h| h.owner.clone());
                let disagrees = if holder_owner != owner {
                    Some("owner")
                } else if name.is_some_and(|n| fingerprint(n) != had.name) {
                    Some("toolCallName")
                } else if parent.is_some_and(|p| *p != had.parent) {
                    Some("parentMessageId")
                } else {
                    None
                };
                match disagrees {
                    Some(field) => Err(ProducerViolation::new(
                        ProducerRule::OwnerMismatch,
                        format!(
                            "{event_type} reopens tool call '{id}' of the input history with a \
                             different {field} (TOOL-07)",
                            id = shown(id.as_ref())
                        ),
                    )),
                    None => Ok(owner),
                }
            }
        }
    }

    fn open_call(
        &mut self,
        id: &ToolCallId,
        name: u64,
        parent: Option<&MessageId>,
        owner: &Owner,
        chunk: bool,
    ) {
        let seq = self.opened(Opened::Call(id.clone()));
        // A reopening that names no parent leaves the call in its message:
        // the one it was opened in, or the input history's.
        let parent = parent
            .or_else(|| self.calls.get(id).and_then(|call| call.parent.as_ref()))
            .or_else(|| self.history_calls.get(id).map(|had| &had.parent))
            .cloned();
        let previous = self.calls.insert(
            id.clone(),
            Call {
                owner: owner.clone(),
                open: true,
                chunk,
                seq,
                name,
                parent: parent.clone(),
            },
        );
        if previous.is_none() {
            self.calls_order.push(id.clone());
        }
        // A parent nothing opened is the assistant message the reducer mints
        // for it, under the call's owner: its id is taken (BASIC-15), a later
        // call under it agrees with that owner (SUB-26, TOOL-02), and a text
        // opener reopens it.
        if let Some(parent) = &parent {
            if !self.messages.contains_key(parent) {
                let seq = self.next_seq();
                self.text.insert(
                    parent.clone(),
                    Item {
                        owner: owner.clone(),
                        open: false,
                        chunk: false,
                        seq,
                        role: Some(TextMessageRole::Assistant),
                        name: None,
                        minted: true,
                    },
                );
                self.messages.insert(parent.clone(), MessageKind::Text);
            }
        }
    }

    fn tool_continue(&mut self, id: &ToolCallId, tag: &Owner, end: bool) -> Verdict {
        let event_type = if end {
            "TOOL_CALL_END"
        } else {
            "TOOL_CALL_ARGS"
        };
        let owner = match self.calls.get(id) {
            Some(call) if call.open && call.chunk => {
                return refuse(
                    ProducerRule::ChunkDiscipline,
                    format!(
                        "{event_type} for tool call '{id}', which is assembling from chunks \
                         (STRM-09)",
                        id = shown(id.as_ref())
                    ),
                )
            }
            Some(call) if call.open => call.owner.clone(),
            _ => {
                return refuse(
                    ProducerRule::NotOpen,
                    format!(
                        "{event_type} for tool call '{id}', which is not open (STRM-02)",
                        id = shown(id.as_ref())
                    ),
                )
            }
        };
        if tag.is_some() && *tag != owner {
            return refuse(
                ProducerRule::OwnerMismatch,
                format!(
                    "{event_type} for tool call '{id}' is attributed to {}, but TOOL_CALL_START \
                     belongs to {} (SUB-21)",
                    owner_text(tag),
                    owner_text(&owner),
                    id = shown(id.as_ref())
                ),
            );
        }
        self.attribute(tag);
        self.close_lane(tag);
        if end {
            if let Some(call) = self.calls.get_mut(id) {
                call.open = false;
            }
            self.closed_explicit(&owner);
        }
        Ok(())
    }

    fn tool_result(
        &mut self,
        message_id: &MessageId,
        call_id: &ToolCallId,
        tag: &Owner,
    ) -> Verdict {
        self.check_new_message(message_id, "TOOL_CALL_RESULT")?;
        let name = self
            .calls
            .get(call_id)
            .map(|call| call.name)
            .or_else(|| self.history_calls.get(call_id).map(|had| had.name));
        if name.is_some_and(|name| self.frontend.contains(&name)) {
            return refuse(
                ProducerRule::Outcome,
                format!(
                    "TOOL_CALL_RESULT answers the frontend tool call '{call_id}'; the client \
                     answers it in the next run's messages (TOOL-24)",
                    call_id = shown(call_id.as_ref())
                ),
            );
        }
        self.attribute(tag);
        // A result naming a call that is assembling ends that call's lane.
        let lane = self
            .assembling(Kind::Tool, call_id.as_str())
            .unwrap_or_else(|| tag.clone());
        self.close_lane(&lane);
        self.messages
            .insert(message_id.clone(), MessageKind::ToolResult);
        self.answered.insert(call_id.clone());
        Ok(())
    }

    /// Resolves the lane an id-less continuation continues: its own owner's
    /// lane, holding a stream of `kind` (STRM-11..STRM-14, STRM-25, STRM-26).
    fn continuation_lane(
        &self,
        kind: Kind,
        tag: &Owner,
    ) -> std::result::Result<Lane, ProducerViolation> {
        if let Some(lane) = self.lane(tag).filter(|lane| lane.kind() == kind) {
            return Ok(lane.clone());
        }
        let first = format!(
            "{} without {} continues no open chunk stream of {}; the first chunk of an item \
             carries its {} ({})",
            kind.chunk_type(),
            kind.id_field(),
            owner_text(tag),
            kind.id_field(),
            kind.rule()
        );
        let message = match tag {
            Some(id) if matches!(self.subagents.get(id), Some(Sub::Ended)) => format!(
                "{first}; subagent '{id}' has ended, and its chunk stream ended with it (STRM-26)",
                id = shown(id.as_ref())
            ),
            None if self.lanes.values().any(|lane| lane.kind() == kind) => format!(
                "{first}; a continuation of a subagent's chunk stream carries its subagentRunId \
                 (STRM-25)"
            ),
            _ => first,
        };
        Err(ProducerViolation::new(
            ProducerRule::ChunkDiscipline,
            message,
        ))
    }

    /// A chunk naming `id` that some lane assembles: it must be its owner's
    /// lane (an untagged chunk continues whatever the opener owns).
    fn check_named_continuation(
        &self,
        kind: Kind,
        id: &str,
        tag: &Owner,
    ) -> std::result::Result<Option<Owner>, ProducerViolation> {
        match self.assembling(kind, id) {
            Some(owner) if tag.is_some() && *tag != owner => Err(ProducerViolation::new(
                ProducerRule::OwnerMismatch,
                format!(
                    "{} for {} '{id}' is attributed to {}, but its chunk stream belongs to {} \
                     (SUB-18, SUB-21)",
                    kind.chunk_type(),
                    kind.id_field(),
                    owner_text(tag),
                    owner_text(&owner),
                    id = shown(id)
                ),
            )),
            other => Ok(other),
        }
    }

    /// An ended invocation's lane takes no new chunk stream (STRM-26).
    fn check_lane_alive(&self, kind: Kind, tag: &Owner) -> Verdict {
        if let Some(id) = tag {
            if matches!(self.subagents.get(id), Some(Sub::Ended)) {
                return refuse(
                    ProducerRule::ChunkDiscipline,
                    format!(
                        "{} opens a chunk stream for subagent '{id}', which has ended; nothing \
                         would end it (STRM-26)",
                        kind.chunk_type(),
                        id = shown(id.as_ref())
                    ),
                );
            }
        }
        Ok(())
    }

    fn text_chunk(&mut self, e: &crate::text::TextMessageChunk, tag: &Owner) -> Verdict {
        let name = e.name.as_deref().map(fingerprint);
        let (id, owner) = match &e.message_id {
            Some(id) => match self.check_named_continuation(Kind::Text, id.as_str(), tag)? {
                Some(owner) => (id.clone(), owner),
                None => {
                    // Opens a chunk stream in the tag's lane.
                    self.check_lane_alive(Kind::Text, tag)?;
                    let role = e.role.unwrap_or(TextMessageRole::Assistant);
                    self.check_text_opener(id, tag, Some(role), name, "TEXT_MESSAGE_CHUNK")?;
                    self.attribute(tag);
                    self.close_lane(tag);
                    let seq = self.opened(Opened::Text(id.clone()));
                    self.text.insert(
                        id.clone(),
                        Item {
                            owner: tag.clone(),
                            open: true,
                            chunk: true,
                            seq,
                            role: Some(role),
                            name,
                            minted: false,
                        },
                    );
                    self.messages.entry(id.clone()).or_insert(MessageKind::Text);
                    self.set_lane(tag, Lane::Text(id.clone()));
                    return Ok(());
                }
            },
            None => match self.continuation_lane(Kind::Text, tag)? {
                Lane::Text(id) => (id, tag.clone()),
                // Unreachable: `continuation_lane` filters by kind; refuse rather than panic.
                _ => return refuse(ProducerRule::ChunkDiscipline, "chunk lane kind mismatch"),
            },
        };
        // A continuation: repeated opener fields agree (STRM-17, STRM-19).
        if let Some(item) = self.text.get(&id) {
            if let Some(role) = e.role {
                if item.role != Some(role) {
                    return refuse(
                        ProducerRule::ChunkDiscipline,
                        format!(
                            "TEXT_MESSAGE_CHUNK for message '{id}' repeats role '{}', but the \
                             message is '{}' (STRM-19, CHG-01)",
                            role_text(role),
                            role_text(item.role.unwrap_or(TextMessageRole::Assistant)),
                            id = shown(id.as_ref())
                        ),
                    );
                }
            }
            if name.is_some() && name != item.name {
                return refuse(
                    ProducerRule::ChunkDiscipline,
                    format!(
                        "TEXT_MESSAGE_CHUNK for message '{id}' repeats a different name \
                         (STRM-19, CHG-01)",
                        id = shown(id.as_ref())
                    ),
                );
            }
        }
        self.attribute(&owner);
        Ok(())
    }

    fn tool_chunk(&mut self, e: &crate::tool::ToolCallChunk, tag: &Owner) -> Verdict {
        let (id, owner) = match &e.tool_call_id {
            Some(id) => match self.check_named_continuation(Kind::Tool, id.as_str(), tag)? {
                Some(owner) => (id.clone(), owner),
                None => {
                    self.check_lane_alive(Kind::Tool, tag)?;
                    let Some(name) = e.tool_call_name.as_deref() else {
                        return refuse(
                            ProducerRule::ChunkDiscipline,
                            format!(
                                "TOOL_CALL_CHUNK opens tool call '{id}' without toolCallName; the \
                                 first chunk of a call carries both toolCallId and toolCallName \
                                 (STRM-14, STRM-17)",
                                id = shown(id.as_ref())
                            ),
                        );
                    };
                    let owner = self.check_tool_opener(
                        id,
                        Some(name),
                        e.parent_message_id.as_ref(),
                        tag,
                        "TOOL_CALL_CHUNK",
                    )?;
                    self.attribute(&owner);
                    self.close_lane(tag);
                    self.open_call(
                        id,
                        fingerprint(name),
                        e.parent_message_id.as_ref(),
                        &owner,
                        true,
                    );
                    self.set_lane(tag, Lane::Tool(id.clone()));
                    return Ok(());
                }
            },
            None => match self.continuation_lane(Kind::Tool, tag) {
                Ok(Lane::Tool(id)) => (id, tag.clone()),
                // Unreachable: `continuation_lane` filters by kind; refuse rather than panic.
                Ok(_) => return refuse(ProducerRule::ChunkDiscipline, "chunk lane kind mismatch"),
                Err(violation) => {
                    if e.tool_call_name.is_some() {
                        return refuse(
                            ProducerRule::ChunkDiscipline,
                            "TOOL_CALL_CHUNK has toolCallName but no toolCallId; a name alone \
                             cannot open a call (STRM-14)",
                        );
                    }
                    return Err(violation);
                }
            },
        };
        if let Some(call) = self.calls.get(&id) {
            if let Some(name) = e.tool_call_name.as_deref() {
                if fingerprint(name) != call.name {
                    return refuse(
                        ProducerRule::ChunkDiscipline,
                        format!(
                            "TOOL_CALL_CHUNK for tool call '{id}' repeats a different \
                             toolCallName (STRM-19, CHG-01)",
                            id = shown(id.as_ref())
                        ),
                    );
                }
            }
            if let Some(parent) = &e.parent_message_id {
                if call.parent.as_ref() != Some(parent) {
                    return refuse(
                        ProducerRule::ChunkDiscipline,
                        format!(
                            "TOOL_CALL_CHUNK for tool call '{id}' repeats a different \
                             parentMessageId (STRM-19, CHG-01)",
                            id = shown(id.as_ref())
                        ),
                    );
                }
            }
        }
        self.attribute(&owner);
        Ok(())
    }

    fn reasoning_chunk(&mut self, id: Option<&MessageId>, tag: &Owner) -> Verdict {
        let owner = match id {
            Some(id) => match self.check_named_continuation(Kind::Reasoning, id.as_str(), tag)? {
                Some(owner) => owner,
                None => {
                    self.check_lane_alive(Kind::Reasoning, tag)?;
                    self.check_reasoning_opener(id, tag, "REASONING_MESSAGE_CHUNK")?;
                    self.attribute(tag);
                    self.close_lane(tag);
                    self.open_reasoning(id, tag, true);
                    self.set_lane(tag, Lane::Reasoning(id.clone()));
                    return Ok(());
                }
            },
            None => {
                self.continuation_lane(Kind::Reasoning, tag)?;
                tag.clone()
            }
        };
        self.attribute(&owner);
        Ok(())
    }

    fn set_lane(&mut self, owner: &Owner, lane: Lane) {
        match owner {
            None => self.parent_lane = Some(lane),
            Some(id) => {
                match &lane {
                    Lane::Text(m) | Lane::Reasoning(m) => {
                        self.lane_messages.insert(m.clone(), id.clone());
                    }
                    Lane::Tool(c) => {
                        self.lane_calls.insert(c.clone(), id.clone());
                    }
                }
                if let Some(old) = self.lanes.insert(id.clone(), lane) {
                    self.unindex(&old);
                }
            }
        }
    }

    /// Drops the content-id index entry of a lane that left `lanes`.
    fn unindex(&mut self, lane: &Lane) {
        match lane {
            Lane::Text(m) | Lane::Reasoning(m) => {
                self.lane_messages.remove(m);
            }
            Lane::Tool(c) => {
                self.lane_calls.remove(c);
            }
        }
    }

    fn step_started(&mut self, name: &str, tag: &Owner) -> Verdict {
        if let Some(holders) = self.steps.get(name) {
            if holders.contains_key(tag) {
                return refuse(
                    ProducerRule::AlreadyOpen,
                    format!(
                        "STEP_STARTED '{name}' while {} holds that step open (LIFE-49)",
                        owner_text(tag),
                        name = shown(name)
                    ),
                );
            }
        }
        self.attribute(tag);
        self.close_lane(tag);
        let seq = self.next_seq();
        match self.steps.get_mut(name) {
            Some(holders) => {
                holders.insert(tag.clone(), seq);
            }
            None => {
                self.steps
                    .insert(name.to_owned(), HashMap::from([(tag.clone(), seq)]));
            }
        }
        self.opened_explicit(tag);
        Ok(())
    }

    fn step_finished(&mut self, name: &str, tag: &Owner) -> Verdict {
        let holders = self.steps.get(name);
        if !holders.is_some_and(|h| h.contains_key(tag)) {
            // Error path only: name the earliest holder.
            let first = holders.and_then(|h| h.iter().min_by_key(|(_, seq)| **seq));
            return match first {
                Some((owner, _)) => refuse(
                    ProducerRule::OwnerMismatch,
                    format!(
                        "STEP_FINISHED '{name}' is attributed to {}, but the open step belongs \
                         to {} (SUB-22)",
                        owner_text(tag),
                        owner_text(owner),
                        name = shown(name)
                    ),
                ),
                None => refuse(
                    ProducerRule::NotOpen,
                    format!(
                        "STEP_FINISHED '{name}' for a step that is not open (LIFE-50)",
                        name = shown(name)
                    ),
                ),
            };
        }
        self.attribute(tag);
        self.close_lane(tag);
        if let Some(holders) = self.steps.get_mut(name) {
            holders.remove(tag);
            if holders.is_empty() {
                self.steps.remove(name);
            }
        }
        self.closed_explicit(tag);
        Ok(())
    }

    fn subagent_started(&mut self, e: &crate::subagent::SubagentStarted) -> Verdict {
        let id = &e.subagent_run_id;
        match self.subagents.get(id) {
            Some(Sub::Active { .. }) => {
                return refuse(
                    ProducerRule::SubagentIdentity,
                    format!(
                        "SUBAGENT_STARTED '{id}' while that invocation is active (SUB-08)",
                        id = shown(id.as_ref())
                    ),
                )
            }
            Some(Sub::Ended) => {
                return refuse(
                    ProducerRule::SubagentIdentity,
                    format!(
                        "SUBAGENT_STARTED '{id}' reuses the id of an invocation already ended in \
                         this run (SUB-03, SUB-09)",
                        id = shown(id.as_ref())
                    ),
                )
            }
            Some(Sub::Attributed) => {
                return refuse(
                    ProducerRule::AttributedBeforeAnnounced,
                    format!(
                        "SUBAGENT_STARTED '{id}' after events attributed to it; announce an \
                         invocation before attributing anything to it (SUB-07)",
                        id = shown(id.as_ref())
                    ),
                )
            }
            None => {}
        }
        if let Some(parent) = &e.parent_subagent_run_id {
            if !matches!(
                self.subagents.get(parent),
                Some(Sub::Active { .. } | Sub::Ended)
            ) {
                return refuse(
                    ProducerRule::SubagentIdentity,
                    format!(
                        "SUBAGENT_STARTED '{id}' names parent invocation '{parent}', which was not \
                         announced in this run (SUB-28)",
                        id = shown(id.as_ref()),
                        parent = shown(parent.as_ref())
                    ),
                );
            }
        }
        let seq = self.opened(Opened::Subagent(id.clone()));
        self.subagents.insert(id.clone(), Sub::Active { seq });
        self.active_subagents += 1;
        Ok(())
    }

    fn subagent_ended(&mut self, id: &SubagentRunId, event_type: &str, suspended: bool) -> Verdict {
        if !matches!(self.subagents.get(id), Some(Sub::Active { .. })) {
            return refuse(
                ProducerRule::NotOpen,
                format!(
                    "{event_type} '{id}' for an invocation that is not active (SUB-10)",
                    id = shown(id.as_ref())
                ),
            );
        }
        // `SUBAGENT_ERROR` ends the invocation like `SUBAGENT_FINISHED`; it
        // closes nothing the invocation opened (subagents.mdx).
        let still_open = self.open_owned.get(id).copied().unwrap_or(0);
        if still_open > 0 {
            return refuse(
                ProducerRule::OpenAtFinish,
                format!(
                    "{event_type} '{id}' with {still_open} of its items still open; an item a \
                     subagent opened closes no later than its terminal (STRM-26)",
                    id = shown(id.as_ref())
                ),
            );
        }
        let owner = Some(id.clone());
        self.close_lane(&owner);
        self.subagents.insert(id.clone(), Sub::Ended);
        self.active_subagents -= 1;
        self.suspended |= suspended;
        Ok(())
    }

    fn activity_snapshot(&mut self, e: &crate::activity::ActivitySnapshot, tag: &Owner) -> Verdict {
        match self.activity.get(&e.message_id) {
            Some(owner) => {
                if e.replace == Some(false) && tag.is_some() && tag != owner {
                    return refuse(
                        ProducerRule::OwnerMismatch,
                        format!(
                            "ACTIVITY_SNAPSHOT replace:false for '{}' is attributed to {}, but the \
                             activity belongs to {} (ACT-08)",
                            shown(e.message_id.as_str()),
                            owner_text(tag),
                            owner_text(owner)
                        ),
                    );
                }
            }
            None => self.check_new_message(&e.message_id, "ACTIVITY_SNAPSHOT")?,
        }
        self.attribute(tag);
        // A replacing snapshot re-mints the activity: its owner becomes the
        // snapshot's own, the parent agent when untagged. A `replace: false`
        // snapshot keeps the established owner.
        let restates_owner = e.replace != Some(false);
        match self.activity.get_mut(&e.message_id) {
            Some(owner) => {
                if restates_owner {
                    *owner = tag.clone();
                }
            }
            None => {
                self.activity.insert(e.message_id.clone(), tag.clone());
                self.messages
                    .insert(e.message_id.clone(), MessageKind::Activity);
            }
        }
        Ok(())
    }

    fn activity_delta(&mut self, e: &crate::activity::ActivityDelta, tag: &Owner) -> Verdict {
        let Some(owner) = self.activity.get(&e.message_id) else {
            return refuse(
                ProducerRule::NoBaseline,
                format!(
                    "ACTIVITY_DELTA for '{}', which no ACTIVITY_SNAPSHOT created (ACT-11, SNAP-07)",
                    shown(e.message_id.as_str())
                ),
            );
        };
        if tag.is_some() && tag != owner {
            return refuse(
                ProducerRule::OwnerMismatch,
                format!(
                    "ACTIVITY_DELTA for '{}' is attributed to {}, but the activity belongs to {} \
                     (SUB-21)",
                    shown(e.message_id.as_str()),
                    owner_text(tag),
                    owner_text(owner)
                ),
            );
        }
        check_activity_root(&e.patch)?;
        self.attribute(tag);
        Ok(())
    }

    fn messages_snapshot(&mut self, messages: &[Message]) -> Verdict {
        let mut seen: HashSet<&MessageId> = HashSet::with_capacity(messages.len());
        for message in messages {
            if !seen.insert(message.id()) {
                return refuse(
                    ProducerRule::DuplicateMessageId,
                    format!(
                        "MESSAGES_SNAPSHOT lists messageId '{}' twice (BASIC-15)",
                        shown(message.id().as_str())
                    ),
                );
            }
            // A frontend call this run made is answered by the client, never
            // by a fabricated tool message (TOOL-24). The client's answer the
            // input history holds may be restated, as the answer to the
            // call it answered there and no other.
            if let Message::Tool(tool) = message {
                let restated = self.history_answers.get(&tool.id) == Some(&tool.tool_call_id);
                if !restated
                    && self
                        .calls
                        .get(&tool.tool_call_id)
                        .is_some_and(|call| self.frontend.contains(&call.name))
                {
                    return refuse(
                        ProducerRule::Outcome,
                        format!(
                            "MESSAGES_SNAPSHOT restates a tool message answering the frontend \
                             tool call '{}'; the client answers it in the next run's messages \
                             (TOOL-24)",
                            shown(tool.tool_call_id.as_str())
                        ),
                    );
                }
            }
        }
        self.close_all_lanes();
        for message in messages {
            // The restated message is the one a reopening agrees with from
            // now on (TEXT-06, BASIC-15), not what this run opened under the
            // id before; an item still open keeps its run state.
            // An input history message stays one: its restatement is still
            // the client's (TOOL-24) and reopens the same way.
            self.messages
                .entry(message.id().clone())
                .and_modify(|kind| {
                    if *kind != MessageKind::History {
                        *kind = MessageKind::Snapshot;
                    }
                })
                .or_insert(MessageKind::Snapshot);
            // A closed item restated as the same kind stays tracked, taking
            // the restatement's owner, role and name, so a reopen is still
            // checked against all three; restated as another kind it goes.
            let held = Held::of(message);
            if let Some(item) = self.text.get_mut(message.id()).filter(|item| !item.open) {
                match held.as_ref().and_then(|h| h.role) {
                    Some(role) => {
                        item.owner = message.subagent_run_id().cloned();
                        item.role = Some(role);
                        item.name = restated_name(message).map(fingerprint);
                        item.minted = false;
                    }
                    None => {
                        self.text.remove(message.id());
                    }
                }
            }
            if let Some(item) = self
                .reasoning
                .get_mut(message.id())
                .filter(|item| !item.open)
            {
                if matches!(message, Message::Reasoning(_)) {
                    item.owner = message.subagent_run_id().cloned();
                } else {
                    self.reasoning.remove(message.id());
                }
            }
            match held {
                Some(held) => {
                    self.held.insert(message.id().clone(), held);
                }
                None => {
                    self.held.remove(message.id());
                }
            }
            if let Message::Activity(activity) = message {
                self.activity
                    .insert(activity.id.clone(), activity.subagent_run_id.clone());
            }
        }
        Ok(())
    }

    fn closers(&self, keep: &mut dyn FnMut(Holder<'_>) -> bool, out: &mut Vec<Event>) {
        let mut open: Vec<(u64, Event)> = Vec::new();
        for (seq, opened) in &self.open_order {
            #[cfg(test)]
            self.closer_probes
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let seq = *seq;
            let closer = match opened {
                Opened::Text(id) => match self.text.get(id) {
                    Some(item) if item.open && item.seq == seq && !item.chunk => {
                        if !keep(Holder::Owned(item.owner.as_ref())) {
                            continue;
                        }
                        let mut end = TextMessageEnd::new(id.clone());
                        end.subagent_run_id = item.owner.clone();
                        end.into()
                    }
                    _ => continue,
                },
                Opened::Reasoning(id) => match self.reasoning.get(id) {
                    Some(item) if item.open && item.seq == seq && !item.chunk => {
                        if !keep(Holder::Owned(item.owner.as_ref())) {
                            continue;
                        }
                        let mut end = ReasoningMessageEnd::new(id.clone());
                        end.subagent_run_id = item.owner.clone();
                        end.into()
                    }
                    _ => continue,
                },
                Opened::Span(id) => match self.spans.get(id) {
                    Some(item) if item.open && item.seq == seq => {
                        if !keep(Holder::Owned(item.owner.as_ref())) {
                            continue;
                        }
                        let mut end = ReasoningEnd::new(id.clone());
                        end.subagent_run_id = item.owner.clone();
                        end.into()
                    }
                    _ => continue,
                },
                Opened::Call(id) => match self.calls.get(id) {
                    Some(call) if call.open && call.seq == seq && !call.chunk => {
                        if !keep(Holder::Owned(call.owner.as_ref())) {
                            continue;
                        }
                        let mut end = ToolCallEnd::new(id.clone());
                        end.subagent_run_id = call.owner.clone();
                        end.into()
                    }
                    _ => continue,
                },
                Opened::Subagent(id) => match self.subagents.get(id) {
                    Some(Sub::Active { seq: s }) if *s == seq => {
                        if !keep(Holder::Invocation(id)) {
                            continue;
                        }
                        SubagentFinished::new(id.clone()).into()
                    }
                    _ => continue,
                },
            };
            open.push((seq, closer));
        }
        for (name, holders) in &self.steps {
            for (owner, seq) in holders {
                if !keep(Holder::Owned(owner.as_ref())) {
                    continue;
                }
                let mut finished = StepFinished::new(name.clone());
                finished.subagent_run_id = owner.clone();
                open.push((*seq, finished.into()));
            }
        }
        open.sort_unstable_by_key(|(seq, _)| std::cmp::Reverse(*seq));
        out.extend(open.into_iter().map(|(_, event)| event));
    }
}

/// Who holds an open item, for [`ProducerVerifier::closers_where`].
#[derive(Clone, Copy)]
#[cfg_attr(not(feature = "server"), allow(dead_code))]
pub(crate) enum Holder<'a> {
    /// An item attributed to this invocation, or to none.
    Owned(Option<&'a SubagentRunId>),
    /// A still-active invocation itself.
    Invocation(&'a SubagentRunId),
}

/// The families [`ProducerState::explicit_continue`] serves.
#[derive(Debug, Clone, Copy)]
enum Family {
    Text,
    Reasoning,
    Span,
}

impl Family {
    const fn noun(self) -> &'static str {
        match self {
            Family::Text => "message",
            Family::Reasoning => "reasoning message",
            Family::Span => "reasoning span",
        }
    }

    const fn event_type(self, end: bool) -> &'static str {
        match (self, end) {
            (Family::Text, false) => "TEXT_MESSAGE_CONTENT",
            (Family::Text, true) => "TEXT_MESSAGE_END",
            (Family::Reasoning, false) => "REASONING_MESSAGE_CONTENT",
            (Family::Reasoning, true) => "REASONING_MESSAGE_END",
            (Family::Span, _) => "REASONING_END",
        }
    }

    const fn not_open_rule(self) -> &'static str {
        match self {
            Family::Text | Family::Reasoning => "STRM-02",
            Family::Span => "REAS-04",
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn pointers_follow_rfc_6901() {
        for good in ["", "/", "/a", "/a/0", "/~0", "/~1x", "//"] {
            assert!(is_pointer(good), "{good:?}");
        }
        for bad in ["a", "/~", "/~2", "a/b", "/x~"] {
            assert!(!is_pointer(bad), "{bad:?}");
        }
    }

    fn ev(value: serde_json::Value) -> Event {
        serde_json::from_value(value).unwrap()
    }

    /// Probes made by the `TOOL_CALL_RESULT` of a parent call while `n`
    /// subagent lanes hold pending chunk streams.
    fn result_probes(n: usize) -> usize {
        use serde_json::json;
        use std::sync::atomic::Ordering;
        let mut v = ProducerVerifier::new();
        let mut run = vec![
            json!({"type": "RUN_STARTED", "threadId": "t", "runId": "r"}),
            json!({"type": "TOOL_CALL_START", "toolCallId": "c1", "toolCallName": "x"}),
            json!({"type": "TOOL_CALL_END", "toolCallId": "c1"}),
        ];
        for i in 0..n {
            run.push(json!({"type": "SUBAGENT_STARTED", "subagentRunId": format!("sub-{i}"), "name": "h"}));
            run.push(json!({"type": "TEXT_MESSAGE_CHUNK", "messageId": format!("m-{i}"), "delta": "x", "subagentRunId": format!("sub-{i}")}));
        }
        for e in run {
            v.check(&ev(e)).unwrap();
        }
        v.state.lane_probes.store(0, Ordering::Relaxed);
        v.check(&ev(json!({"type": "TOOL_CALL_RESULT", "messageId": "r1", "toolCallId": "c1", "content": "ok"})))
            .unwrap();
        let probes = v.state.lane_probes.load(Ordering::Relaxed);
        // Behaviour: continuations still resolve through the lanes.
        if n > 5 {
            let chunk = |m: &str, sub: Option<&str>| {
                let mut e = json!({"type": "TEXT_MESSAGE_CHUNK", "messageId": m, "delta": "y"});
                if let Some(s) = sub {
                    e["subagentRunId"] = json!(s);
                }
                ev(e)
            };
            assert!(v.check(&chunk("m-0", Some("sub-0"))).is_ok());
            let err = v.check(&chunk("m-0", Some("sub-1"))).unwrap_err();
            assert_eq!(err.rule, ProducerRule::OwnerMismatch);
            assert!(v.check(&chunk("m-5", None)).is_ok());
        }
        probes
    }

    /// Entries `closers` examines for the one open message after `n`
    /// messages were opened and ended in the run.
    fn closer_probes(n: usize) -> usize {
        use serde_json::json;
        use std::sync::atomic::Ordering;
        let mut v = ProducerVerifier::new();
        v.check(&ev(
            json!({"type": "RUN_STARTED", "threadId": "t", "runId": "r"}),
        ))
        .unwrap();
        for i in 0..n {
            let id = format!("m-{i}");
            v.check(&ev(
                json!({"type": "TEXT_MESSAGE_START", "messageId": id, "role": "assistant"}),
            ))
            .unwrap();
            v.check(&ev(json!({"type": "TEXT_MESSAGE_END", "messageId": id})))
                .unwrap();
        }
        v.check(&ev(
            json!({"type": "TEXT_MESSAGE_START", "messageId": "open", "role": "assistant"}),
        ))
        .unwrap();
        v.state.closer_probes.store(0, Ordering::Relaxed);
        let mut out = Vec::new();
        v.closers(&mut out);
        assert_eq!(out.len(), 1);
        assert!(matches!(&out[0], Event::TextMessageEnd(e) if e.message_id.as_str() == "open"));
        v.state.closer_probes.load(Ordering::Relaxed)
    }

    #[test]
    fn closers_do_not_scan_items_already_ended() {
        let (small, large) = (closer_probes(16), closer_probes(1024));
        assert!(small <= 4, "16 ended messages cost {small} probes");
        assert!(large <= 4, "probes grew with the ended items: {large}");
    }

    #[test]
    fn assembling_does_not_scan_the_subagent_lanes() {
        let (small, large) = (result_probes(16), result_probes(256));
        assert!(small <= 2, "16 lanes cost {small} probes");
        assert_eq!(small, large, "probes grew with the lane count");
    }

    fn feed(v: &mut ProducerVerifier, events: Vec<serde_json::Value>) {
        for e in events {
            let event = ev(e);
            if let Err(violation) = v.check(&event) {
                panic!("{event:?} refused: {violation}");
            }
        }
    }

    /// activity.mdx: a replacing snapshot re-mints the activity, so its
    /// owner becomes the snapshot's own, absence included.
    #[test]
    fn an_untagged_replacing_snapshot_makes_the_parent_the_owner() {
        use serde_json::json;
        let snapshot = |sub: Option<&str>| {
            let mut e = json!({"type": "ACTIVITY_SNAPSHOT", "messageId": "a1",
                               "activityType": "x", "content": {}});
            if let Some(s) = sub {
                e["subagentRunId"] = json!(s);
            }
            e
        };
        let delta = |sub: Option<&str>| {
            let mut e = json!({"type": "ACTIVITY_DELTA", "messageId": "a1", "activityType": "x",
                               "patch": [{"op": "add", "path": "/k", "value": 1}]});
            if let Some(s) = sub {
                e["subagentRunId"] = json!(s);
            }
            ev(e)
        };
        let mut v = ProducerVerifier::new();
        feed(
            &mut v,
            vec![
                json!({"type": "RUN_STARTED", "threadId": "t", "runId": "r"}),
                json!({"type": "SUBAGENT_STARTED", "subagentRunId": "A", "name": "h"}),
                snapshot(Some("A")),
                snapshot(None),
            ],
        );
        let err = v.check(&delta(Some("A"))).unwrap_err();
        assert_eq!(err.rule, ProducerRule::OwnerMismatch);
        v.check(&delta(None)).unwrap();
    }

    fn input(messages: serde_json::Value) -> RunAgentInput {
        serde_json::from_value(serde_json::json!({
            "threadId": "t", "runId": "r", "messages": messages,
            "tools": [], "context": [], "state": {}, "forwardedProps": {}
        }))
        .unwrap()
    }

    /// text-messages.mdx: a producer MAY reopen a held message, the input
    /// history's, a snapshot's or an earlier run's, and the message
    /// continues; only a message of another kind keeps the id (BASIC-15).
    #[test]
    fn a_held_message_reopens_across_runs_history_and_snapshots() {
        use serde_json::json;
        let history = json!([
            {"id": "m1", "role": "assistant", "content": "hi"},
            {"id": "z1", "role": "reasoning", "content": "hmm"},
            {"id": "t1", "role": "tool", "content": "ok", "toolCallId": "c0"}
        ]);
        let mut v = ProducerVerifier::for_input(&input(history.clone()));
        feed(
            &mut v,
            vec![
                json!({"type": "RUN_STARTED", "threadId": "t", "runId": "r"}),
                json!({"type": "TEXT_MESSAGE_START", "messageId": "m1"}),
                json!({"type": "TEXT_MESSAGE_END", "messageId": "m1"}),
                json!({"type": "REASONING_MESSAGE_START", "messageId": "z1", "role": "reasoning"}),
                json!({"type": "REASONING_MESSAGE_END", "messageId": "z1"}),
                json!({"type": "MESSAGES_SNAPSHOT", "messages": [
                    {"id": "s1", "role": "assistant", "content": "x"}
                ]}),
                json!({"type": "TEXT_MESSAGE_START", "messageId": "s1"}),
                json!({"type": "TEXT_MESSAGE_END", "messageId": "s1"}),
            ],
        );
        // A message of another kind still keeps its id.
        let start = |id: &str| ev(json!({"type": "TEXT_MESSAGE_START", "messageId": id}));
        for id in ["t1", "z1"] {
            let err = v.check(&start(id)).unwrap_err();
            assert_eq!(err.rule, ProducerRule::DuplicateMessageId, "{id}");
        }
        let err = v
            .check(&ev(
                json!({"type": "REASONING_MESSAGE_START", "messageId": "m1", "role": "reasoning"}),
            ))
            .unwrap_err();
        assert_eq!(err.rule, ProducerRule::DuplicateMessageId);
        // A held message reopens under its own owner and role only.
        let err = v
            .check(&ev(
                json!({"type": "TEXT_MESSAGE_START", "messageId": "m1", "role": "user"}),
            ))
            .unwrap_err();
        assert_eq!(err.rule, ProducerRule::OwnerMismatch);
        let err = v
            .check(&ev(
                json!({"type": "TEXT_MESSAGE_START", "messageId": "m1", "subagentRunId": "B"}),
            ))
            .unwrap_err();
        assert_eq!(err.rule, ProducerRule::OwnerMismatch);

        // An earlier run's message reopens in a later run.
        let mut v = ProducerVerifier::new();
        feed(
            &mut v,
            vec![
                json!({"type": "RUN_STARTED", "threadId": "t", "runId": "r1"}),
                json!({"type": "TEXT_MESSAGE_START", "messageId": "m1"}),
                json!({"type": "TEXT_MESSAGE_END", "messageId": "m1"}),
                json!({"type": "RUN_FINISHED", "threadId": "t", "runId": "r1"}),
                json!({"type": "RUN_STARTED", "threadId": "t", "runId": "r2"}),
                json!({"type": "TEXT_MESSAGE_START", "messageId": "m1"}),
                json!({"type": "TEXT_MESSAGE_END", "messageId": "m1"}),
            ],
        );
    }

    /// subagents.mdx: a tool call inherits the attribution of the message
    /// holding it, a history or snapshot message's included.
    #[test]
    fn a_call_on_a_held_message_inherits_its_owner() {
        use serde_json::json;
        let history = json!([
            {"id": "m1", "role": "assistant", "content": "hi", "subagentRunId": "A"}
        ]);
        let mut v = ProducerVerifier::for_input(&input(history));
        feed(
            &mut v,
            vec![
                json!({"type": "RUN_STARTED", "threadId": "t", "runId": "r"}),
                json!({"type": "SUBAGENT_STARTED", "subagentRunId": "A", "name": "a"}),
                json!({"type": "SUBAGENT_STARTED", "subagentRunId": "B", "name": "b"}),
            ],
        );
        let err = v
            .check(&ev(
                json!({"type": "TOOL_CALL_START", "toolCallId": "c1", "toolCallName": "x",
                              "parentMessageId": "m1", "subagentRunId": "B"}),
            ))
            .unwrap_err();
        assert_eq!(err.rule, ProducerRule::OwnerMismatch);
        feed(
            &mut v,
            vec![
                json!({"type": "TOOL_CALL_START", "toolCallId": "c1", "toolCallName": "x",
                       "parentMessageId": "m1"}),
                json!({"type": "TOOL_CALL_ARGS", "toolCallId": "c1", "delta": "{}",
                       "subagentRunId": "A"}),
                json!({"type": "MESSAGES_SNAPSHOT", "messages": [
                    {"id": "s1", "role": "assistant", "content": "x", "subagentRunId": "B"}
                ]}),
                json!({"type": "TOOL_CALL_START", "toolCallId": "c2", "toolCallName": "x",
                       "parentMessageId": "s1"}),
                json!({"type": "TOOL_CALL_END", "toolCallId": "c2", "subagentRunId": "B"}),
            ],
        );
    }

    /// Ids in violation text are escaped and previewed, and long lists are
    /// counted, as in the consumer's error text.
    #[test]
    fn violation_text_bounds_ids() {
        use serde_json::json;
        let forged = "m\n\u{1b}[31mX";
        let long = "y".repeat(1 << 16);
        let mut v = ProducerVerifier::for_input(&input(json!([
            {"id": forged, "role": "user", "content": "hi"}
        ])));
        feed(
            &mut v,
            vec![json!({"type": "RUN_STARTED", "threadId": "t", "runId": "r"})],
        );
        let err = v
            .check(&ev(json!({"type": "TOOL_CALL_RESULT", "messageId": forged,
                              "toolCallId": long, "content": "c"})))
            .unwrap_err();
        assert_eq!(err.rule, ProducerRule::DuplicateMessageId);
        assert!(
            !err.message.contains('\n') && !err.message.contains('\u{1b}'),
            "{:?}",
            err.message
        );
        let err = v
            .check(&ev(json!({"type": "TEXT_MESSAGE_END", "messageId": long})))
            .unwrap_err();
        assert!(err.message.len() < 1024, "{} bytes", err.message.len());
        for i in 0..100 {
            let start =
                json!({"type": "TEXT_MESSAGE_START", "messageId": format!("o{i:03}{long}")});
            v.check(&ev(start)).unwrap();
        }
        let err = v
            .check(&ev(
                json!({"type": "RUN_FINISHED", "threadId": "t", "runId": "r"}),
            ))
            .unwrap_err();
        assert_eq!(err.rule, ProducerRule::OpenAtFinish);
        assert!(err.message.len() < 4096, "{} bytes", err.message.len());
        assert!(
            err.message.contains(", and 84 more"),
            "{}",
            &err.message[..256]
        );
    }

    /// A table inflated by one run is dropped, not cleared at O(capacity),
    /// once later runs are small.
    #[test]
    fn an_oversized_run_table_is_dropped_after_a_small_run() {
        use serde_json::json;
        let floor = crate::pipeline::RETAIN_FLOOR;
        let mut v = ProducerVerifier::new();
        v.check(&ev(
            json!({"type": "RUN_STARTED", "threadId": "t", "runId": "r1"}),
        ))
        .unwrap();
        for i in 0..100_000 {
            let id = format!("m{i}");
            v.check(&ev(json!({"type": "TEXT_MESSAGE_START", "messageId": id})))
                .unwrap();
            let call = format!("c{i}");
            v.check(&ev(
                json!({"type": "TOOL_CALL_START", "toolCallId": call, "toolCallName": "x"}),
            ))
            .unwrap();
        }
        v.check(&ev(json!({"type": "RUN_ERROR", "message": "boom"})))
            .unwrap();
        feed(
            &mut v,
            vec![
                json!({"type": "RUN_STARTED", "threadId": "t", "runId": "r2"}),
                json!({"type": "TEXT_MESSAGE_START", "messageId": "only"}),
                json!({"type": "TEXT_MESSAGE_END", "messageId": "only"}),
                json!({"type": "RUN_FINISHED", "threadId": "t", "runId": "r2"}),
            ],
        );
        let s = &v.state;
        assert!(s.text.capacity() <= floor, "text {}", s.text.capacity());
        assert!(s.calls.capacity() <= floor, "calls {}", s.calls.capacity());
        assert!(
            s.calls_order.capacity() <= floor,
            "order {}",
            s.calls_order.capacity()
        );
    }

    /// streaming.mdx: an item a subagent opened closes no later than its
    /// terminal event, `SUBAGENT_ERROR` included (STRM-26).
    #[test]
    fn subagent_error_with_an_open_item_is_refused() {
        use serde_json::json;
        let mut v = ProducerVerifier::new();
        feed(
            &mut v,
            vec![
                json!({"type": "RUN_STARTED", "threadId": "t", "runId": "r"}),
                json!({"type": "SUBAGENT_STARTED", "subagentRunId": "s1", "name": "h"}),
                json!({"type": "TEXT_MESSAGE_START", "messageId": "m1", "subagentRunId": "s1"}),
            ],
        );
        let err = v
            .check(&ev(
                json!({"type": "SUBAGENT_ERROR", "subagentRunId": "s1", "message": "x"}),
            ))
            .unwrap_err();
        assert_eq!(err.rule, ProducerRule::OpenAtFinish);
        assert!(err.message.starts_with("SUBAGENT_ERROR"), "{}", err.message);
        feed(
            &mut v,
            vec![
                json!({"type": "TEXT_MESSAGE_END", "messageId": "m1", "subagentRunId": "s1"}),
                json!({"type": "SUBAGENT_ERROR", "subagentRunId": "s1", "message": "x"}),
                json!({"type": "RUN_FINISHED", "threadId": "t", "runId": "r"}),
            ],
        );
    }

    /// A parent nothing opened becomes the assistant message the reducer
    /// mints for it: its id is taken (BASIC-15) and later calls under it
    /// agree with its owner (SUB-26, TOOL-02).
    #[test]
    fn a_minted_parent_is_tracked() {
        use serde_json::json;
        let base = || {
            let mut v = ProducerVerifier::new();
            feed(
                &mut v,
                vec![
                    json!({"type": "RUN_STARTED", "threadId": "t", "runId": "r"}),
                    json!({"type": "SUBAGENT_STARTED", "subagentRunId": "A", "name": "a"}),
                    json!({"type": "SUBAGENT_STARTED", "subagentRunId": "B", "name": "b"}),
                    json!({"type": "TOOL_CALL_START", "toolCallId": "c1", "toolCallName": "x",
                           "parentMessageId": "P", "subagentRunId": "A"}),
                    json!({"type": "TOOL_CALL_END", "toolCallId": "c1"}),
                ],
            );
            v
        };
        let mut v = base();
        let err = v
            .check(&ev(json!({"type": "TOOL_CALL_RESULT", "messageId": "P",
                              "toolCallId": "c1", "content": "ok"})))
            .unwrap_err();
        assert_eq!(err.rule, ProducerRule::DuplicateMessageId);
        let err = v
            .check(&ev(
                json!({"type": "REASONING_MESSAGE_START", "messageId": "P", "role": "reasoning"}),
            ))
            .unwrap_err();
        assert_eq!(err.rule, ProducerRule::DuplicateMessageId);
        let err = v
            .check(&ev(
                json!({"type": "TOOL_CALL_START", "toolCallId": "c2", "toolCallName": "x",
                       "parentMessageId": "P", "subagentRunId": "B"}),
            ))
            .unwrap_err();
        assert_eq!(err.rule, ProducerRule::OwnerMismatch);
        feed(
            &mut v,
            vec![
                json!({"type": "TOOL_CALL_START", "toolCallId": "c3", "toolCallName": "x",
                       "parentMessageId": "P"}),
                json!({"type": "TOOL_CALL_ARGS", "toolCallId": "c3", "delta": "{}",
                       "subagentRunId": "A"}),
                json!({"type": "TOOL_CALL_END", "toolCallId": "c3"}),
                json!({"type": "TEXT_MESSAGE_START", "messageId": "P", "subagentRunId": "A"}),
                json!({"type": "TEXT_MESSAGE_END", "messageId": "P"}),
            ],
        );
        // The chunk form mints the same way.
        let mut v = ProducerVerifier::new();
        feed(
            &mut v,
            vec![
                json!({"type": "RUN_STARTED", "threadId": "t", "runId": "r"}),
                json!({"type": "TOOL_CALL_CHUNK", "toolCallId": "c1", "toolCallName": "x",
                       "parentMessageId": "Q"}),
                json!({"type": "CUSTOM", "name": "n", "value": 1}),
            ],
        );
        let err = v
            .check(&ev(json!({"type": "TOOL_CALL_RESULT", "messageId": "Q",
                              "toolCallId": "c1", "content": "ok"})))
            .unwrap_err();
        assert_eq!(err.rule, ProducerRule::DuplicateMessageId);
    }

    /// A snapshot restating a closed text message as text keeps its name
    /// check: a reopen under another name is still refused (TEXT-06).
    #[test]
    fn a_snapshot_restating_a_closed_text_message_keeps_its_name() {
        use serde_json::json;
        let mut v = ProducerVerifier::new();
        feed(
            &mut v,
            vec![
                json!({"type": "RUN_STARTED", "threadId": "t", "runId": "r"}),
                json!({"type": "TEXT_MESSAGE_START", "messageId": "m1", "role": "assistant",
                       "name": "bot"}),
                json!({"type": "TEXT_MESSAGE_END", "messageId": "m1"}),
                json!({"type": "MESSAGES_SNAPSHOT", "messages": [
                    {"id": "m1", "role": "assistant", "name": "bot", "content": "x"}
                ]}),
            ],
        );
        let err = v
            .check(&ev(json!({"type": "TEXT_MESSAGE_START", "messageId": "m1",
                              "role": "assistant", "name": "other"})))
            .unwrap_err();
        assert_eq!(err.rule, ProducerRule::OwnerMismatch);
        feed(
            &mut v,
            vec![
                json!({"type": "TEXT_MESSAGE_START", "messageId": "m1", "role": "assistant",
                       "name": "bot"}),
                json!({"type": "TEXT_MESSAGE_END", "messageId": "m1"}),
            ],
        );
    }

    /// A snapshot restating an id as another kind of message makes the
    /// restated message the one a reopening agrees with (TEXT-06, BASIC-15).
    #[test]
    fn a_snapshot_restating_an_id_judges_reopens_by_the_restatement() {
        use serde_json::json;
        let mut v = ProducerVerifier::new();
        feed(
            &mut v,
            vec![
                json!({"type": "RUN_STARTED", "threadId": "t", "runId": "r"}),
                json!({"type": "TEXT_MESSAGE_START", "messageId": "m1"}),
                json!({"type": "TEXT_MESSAGE_END", "messageId": "m1"}),
                json!({"type": "TOOL_CALL_START", "toolCallId": "c1", "toolCallName": "x"}),
                json!({"type": "TOOL_CALL_END", "toolCallId": "c1"}),
                json!({"type": "TOOL_CALL_RESULT", "messageId": "r1", "toolCallId": "c1",
                       "content": "ok"}),
                json!({"type": "MESSAGES_SNAPSHOT", "messages": [
                    {"id": "m1", "role": "reasoning", "content": "hmm"},
                    {"id": "r1", "role": "assistant", "content": "x"}
                ]}),
            ],
        );
        let err = v
            .check(&ev(
                json!({"type": "TEXT_MESSAGE_START", "messageId": "m1"}),
            ))
            .unwrap_err();
        assert_eq!(err.rule, ProducerRule::DuplicateMessageId);
        feed(
            &mut v,
            vec![
                json!({"type": "REASONING_MESSAGE_START", "messageId": "m1", "role": "reasoning"}),
                json!({"type": "REASONING_MESSAGE_END", "messageId": "m1"}),
                json!({"type": "TEXT_MESSAGE_START", "messageId": "r1"}),
                json!({"type": "TEXT_MESSAGE_END", "messageId": "r1"}),
            ],
        );
    }

    /// tool-calls.mdx: no fabricated tool message for a frontend call, a
    /// snapshot's included (TOOL-24).
    #[test]
    fn a_snapshot_may_not_answer_a_frontend_call() {
        use serde_json::json;
        let input: RunAgentInput = serde_json::from_value(json!({
            "threadId": "t", "runId": "r", "messages": [],
            "tools": [{"name": "ask", "description": "d"}], "context": [],
            "state": {}, "forwardedProps": {}
        }))
        .unwrap();
        let mut v = ProducerVerifier::for_input(&input);
        feed(
            &mut v,
            vec![
                json!({"type": "RUN_STARTED", "threadId": "t", "runId": "r"}),
                json!({"type": "TOOL_CALL_START", "toolCallId": "c1", "toolCallName": "ask"}),
                json!({"type": "TOOL_CALL_END", "toolCallId": "c1"}),
            ],
        );
        let err = v
            .check(&ev(json!({"type": "MESSAGES_SNAPSHOT", "messages": [
                {"id": "a1", "role": "assistant", "content": "x",
                 "toolCalls": [{"id": "c1", "type": "function",
                                "function": {"name": "ask", "arguments": "{}"}}]},
                {"id": "t1", "role": "tool", "content": "fake", "toolCallId": "c1"}
            ]})))
            .unwrap_err();
        assert_eq!(err.rule, ProducerRule::Outcome);
        assert!(!v.state.messages.contains_key("a1"), "a refusal mutated");
    }

    /// tool-calls.mdx: a reopening start agrees with the call it reopens,
    /// a call of the input history included (TOOL-07).
    #[test]
    fn a_history_call_reopens_only_under_its_name() {
        use serde_json::json;
        let history = json!([
            {"id": "a1", "role": "assistant", "content": "x",
             "toolCalls": [{"id": "h1", "type": "function",
                            "function": {"name": "search", "arguments": "{}"}}]}
        ]);
        let mut v = ProducerVerifier::for_input(&input(history));
        feed(
            &mut v,
            vec![json!({"type": "RUN_STARTED", "threadId": "t", "runId": "r"})],
        );
        let err = v
            .check(&ev(
                json!({"type": "TOOL_CALL_START", "toolCallId": "h1", "toolCallName": "delete_all"}),
            ))
            .unwrap_err();
        assert_eq!(err.rule, ProducerRule::OwnerMismatch);
        feed(
            &mut v,
            vec![
                json!({"type": "TOOL_CALL_START", "toolCallId": "h1", "toolCallName": "search"}),
                json!({"type": "TOOL_CALL_END", "toolCallId": "h1"}),
            ],
        );
    }

    /// A reopened call of the input history keeps its parent and owner too
    /// (TOOL-07): the reducer reopens it in place, so a new parent would
    /// never be minted, and its id must stay free.
    #[test]
    fn a_history_call_reopens_only_under_its_parent_and_owner() {
        use serde_json::json;
        let history = json!([
            {"id": "a1", "role": "assistant", "content": "x", "subagentRunId": "S",
             "toolCalls": [{"id": "h1", "type": "function",
                            "function": {"name": "search", "arguments": "{}"}}]}
        ]);
        let start = |extra: serde_json::Value| {
            let mut event = json!({"type": "TOOL_CALL_START", "toolCallId": "h1",
                                   "toolCallName": "search"});
            event
                .as_object_mut()
                .unwrap()
                .extend(extra.as_object().unwrap().clone());
            ev(event)
        };
        let fresh = || {
            let mut v = ProducerVerifier::for_input(&input(history.clone()));
            feed(
                &mut v,
                vec![json!({"type": "RUN_STARTED", "threadId": "t", "runId": "r"})],
            );
            v
        };
        for refused in [
            json!({"parentMessageId": "P", "subagentRunId": "S"}),
            json!({"subagentRunId": "T"}),
            json!({"parentMessageId": "a1", "subagentRunId": "T"}),
            json!({}),
        ] {
            let mut v = fresh();
            let err = v.check(&start(refused.clone())).unwrap_err();
            assert_eq!(err.rule, ProducerRule::OwnerMismatch, "{refused}");
            assert!(!v.state.messages.contains_key("P"), "{refused}: minted P");
        }
        for accepted in [
            json!({"parentMessageId": "a1"}),
            json!({"subagentRunId": "S"}),
            json!({"parentMessageId": "a1", "subagentRunId": "S"}),
        ] {
            let mut v = fresh();
            v.check(&start(accepted.clone()))
                .unwrap_or_else(|e| panic!("{accepted}: {e:?}"));
            feed(
                &mut v,
                vec![json!({"type": "TOOL_CALL_END", "toolCallId": "h1",
                            "subagentRunId": "S"})],
            );
            // Reopened again within the run, it still sits in a1.
            v.check(&start(json!({"parentMessageId": "a1"})))
                .unwrap_or_else(|e| panic!("{accepted} then a1: {e:?}"));
        }
    }

    /// A reopening that names no parent leaves the call where it was, so a
    /// later reopening naming that parent agrees (TOOL-07).
    #[test]
    fn a_reopening_without_a_parent_keeps_the_calls_parent() {
        use serde_json::json;
        let mut v = ProducerVerifier::new();
        feed(
            &mut v,
            vec![
                json!({"type": "RUN_STARTED", "threadId": "t", "runId": "r"}),
                json!({"type": "TOOL_CALL_START", "toolCallId": "c1", "toolCallName": "x",
                       "parentMessageId": "P"}),
                json!({"type": "TOOL_CALL_END", "toolCallId": "c1"}),
                json!({"type": "TOOL_CALL_START", "toolCallId": "c1", "toolCallName": "x"}),
                json!({"type": "TOOL_CALL_END", "toolCallId": "c1"}),
                json!({"type": "TOOL_CALL_START", "toolCallId": "c1", "toolCallName": "x",
                       "parentMessageId": "P"}),
                json!({"type": "TOOL_CALL_END", "toolCallId": "c1"}),
            ],
        );
    }

    /// The first text opener of a minted parent may name it, as the reducer
    /// fills a minted carrier's missing name; owner and role still agree
    /// (TEXT-06).
    #[test]
    fn a_minted_parent_takes_its_first_openers_name() {
        use serde_json::json;
        let minted = || {
            let mut v = ProducerVerifier::new();
            feed(
                &mut v,
                vec![
                    json!({"type": "RUN_STARTED", "threadId": "t", "runId": "r"}),
                    json!({"type": "TOOL_CALL_START", "toolCallId": "c1", "toolCallName": "x",
                           "parentMessageId": "P"}),
                    json!({"type": "TOOL_CALL_END", "toolCallId": "c1"}),
                ],
            );
            v
        };
        let mut v = minted();
        feed(
            &mut v,
            vec![
                json!({"type": "TEXT_MESSAGE_START", "messageId": "P", "role": "assistant",
                       "name": "bot"}),
                json!({"type": "TEXT_MESSAGE_END", "messageId": "P"}),
            ],
        );
        // The name is now established.
        let err = v
            .check(&ev(json!({"type": "TEXT_MESSAGE_START", "messageId": "P",
                              "role": "assistant", "name": "other"})))
            .unwrap_err();
        assert_eq!(err.rule, ProducerRule::OwnerMismatch);
        // The chunk form fills it too.
        let mut v = minted();
        feed(
            &mut v,
            vec![
                json!({"type": "TEXT_MESSAGE_CHUNK", "messageId": "P", "name": "bot",
                       "delta": "hi"}),
                json!({"type": "CUSTOM", "name": "n", "value": 1}),
            ],
        );
        // The role still agrees.
        let mut v = minted();
        let err = v
            .check(&ev(
                json!({"type": "TEXT_MESSAGE_START", "messageId": "P", "role": "user",
                              "name": "bot"}),
            ))
            .unwrap_err();
        assert_eq!(err.rule, ProducerRule::OwnerMismatch);
    }

    /// A snapshot may restate the client's answer to a frontend call of the
    /// input history that this run reopened, as often as it restates the
    /// thread (TOOL-24).
    #[test]
    fn a_snapshot_may_restate_the_history_answer_to_a_frontend_call() {
        use serde_json::json;
        let thread = json!([
            {"id": "a1", "role": "assistant", "content": "x",
             "toolCalls": [{"id": "h1", "type": "function",
                            "function": {"name": "ask", "arguments": "{}"}}]},
            {"id": "t1", "role": "tool", "content": "yes", "toolCallId": "h1"}
        ]);
        let input: RunAgentInput = serde_json::from_value(json!({
            "threadId": "t", "runId": "r", "messages": thread,
            "tools": [{"name": "ask", "description": "d"}], "context": [],
            "state": {}, "forwardedProps": {}
        }))
        .unwrap();
        let mut v = ProducerVerifier::for_input(&input);
        feed(
            &mut v,
            vec![
                json!({"type": "RUN_STARTED", "threadId": "t", "runId": "r"}),
                json!({"type": "TOOL_CALL_START", "toolCallId": "h1", "toolCallName": "ask"}),
                json!({"type": "TOOL_CALL_END", "toolCallId": "h1"}),
                json!({"type": "MESSAGES_SNAPSHOT", "messages": thread}),
                json!({"type": "MESSAGES_SNAPSHOT", "messages": thread}),
            ],
        );
        // A tool message of the run's own making is still refused.
        let err = v
            .check(&ev(json!({"type": "MESSAGES_SNAPSHOT", "messages": [
                {"id": "t2", "role": "tool", "content": "fake", "toolCallId": "h1"}
            ]})))
            .unwrap_err();
        assert_eq!(err.rule, ProducerRule::Outcome);
        // So is one reusing the history answer's id for a call of this run.
        feed(
            &mut v,
            vec![
                json!({"type": "TOOL_CALL_START", "toolCallId": "c1", "toolCallName": "ask"}),
                json!({"type": "TOOL_CALL_END", "toolCallId": "c1"}),
            ],
        );
        let err = v
            .check(&ev(json!({"type": "MESSAGES_SNAPSHOT", "messages": [
                {"id": "t1", "role": "tool", "content": "fake", "toolCallId": "c1"}
            ]})))
            .unwrap_err();
        assert_eq!(err.rule, ProducerRule::Outcome);
    }

    /// The restatement exemption covers only the call the history tool
    /// message answered: its id restated as an answer to another history
    /// frontend call this run reopened is a fabricated answer (TOOL-24).
    #[test]
    fn a_snapshot_may_not_move_a_history_answer_to_another_reopened_call() {
        use serde_json::json;
        let thread = json!([
            {"id": "a1", "role": "assistant", "content": "x",
             "toolCalls": [{"id": "h1", "type": "function",
                            "function": {"name": "ask", "arguments": "{}"}},
                           {"id": "h2", "type": "function",
                            "function": {"name": "ask", "arguments": "{}"}}]},
            {"id": "t1", "role": "tool", "content": "yes", "toolCallId": "h1"}
        ]);
        let input: RunAgentInput = serde_json::from_value(json!({
            "threadId": "t", "runId": "r", "messages": thread,
            "tools": [{"name": "ask", "description": "d"}], "context": [],
            "state": {}, "forwardedProps": {}
        }))
        .unwrap();
        let mut v = ProducerVerifier::for_input(&input);
        feed(
            &mut v,
            vec![
                json!({"type": "RUN_STARTED", "threadId": "t", "runId": "r"}),
                json!({"type": "TOOL_CALL_START", "toolCallId": "h2", "toolCallName": "ask"}),
                json!({"type": "TOOL_CALL_END", "toolCallId": "h2"}),
            ],
        );
        let err = v
            .check(&ev(json!({"type": "MESSAGES_SNAPSHOT", "messages": [
                {"id": "t1", "role": "tool", "content": "fake", "toolCallId": "h2"}
            ]})))
            .unwrap_err();
        assert_eq!(err.rule, ProducerRule::Outcome);
        // The history answer to h1 is still restated as it was.
        feed(
            &mut v,
            vec![json!({"type": "MESSAGES_SNAPSHOT", "messages": thread})],
        );
    }

    /// Steps are indexed by owner. A regression guard for the O(1) per
    /// event lookup (not red before the fix: it pins behaviour, not cost).
    #[test]
    fn many_owners_of_one_step_name_are_indexed() {
        use serde_json::json;
        let mut v = ProducerVerifier::new();
        feed(
            &mut v,
            vec![json!({"type": "RUN_STARTED", "threadId": "t", "runId": "r"})],
        );
        for i in 0..64 {
            feed(
                &mut v,
                vec![
                    json!({"type": "SUBAGENT_STARTED", "subagentRunId": format!("s{i}"), "name": "h"}),
                    json!({"type": "STEP_STARTED", "stepName": "plan", "subagentRunId": format!("s{i}")}),
                ],
            );
        }
        let err = v
            .check(&ev(
                json!({"type": "STEP_STARTED", "stepName": "plan", "subagentRunId": "s7"}),
            ))
            .unwrap_err();
        assert_eq!(err.rule, ProducerRule::AlreadyOpen);
        let err = v
            .check(&ev(json!({"type": "STEP_FINISHED", "stepName": "plan"})))
            .unwrap_err();
        assert_eq!(err.rule, ProducerRule::OwnerMismatch);
        assert!(err.message.contains("'s0'"), "{}", err.message);
        let mut closers = Vec::new();
        v.closers(&mut closers);
        assert!(matches!(&closers[0], Event::StepFinished(f)
            if f.subagent_run_id.as_ref().map(|s| s.as_str()) == Some("s63")));
        for i in 0..64 {
            feed(
                &mut v,
                vec![
                    json!({"type": "STEP_FINISHED", "stepName": "plan", "subagentRunId": format!("s{i}")}),
                    json!({"type": "SUBAGENT_FINISHED", "subagentRunId": format!("s{i}")}),
                ],
            );
        }
        assert!(v.state.steps.is_empty());
        feed(
            &mut v,
            vec![json!({"type": "RUN_FINISHED", "threadId": "t", "runId": "r"})],
        );
    }

    #[test]
    fn violations_display_rule_and_message() {
        let v = ProducerViolation::new(ProducerRule::NotOpen, "x");
        assert_eq!(v.to_string(), "NotOpen: x");
    }
}
