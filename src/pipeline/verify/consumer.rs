//! The consumer-side verifier. See the [module docs](super).
//!
//! # Rules
//!
//! * **Lifecycle** (`events/lifecycle.mdx`). The first event is `RUN_STARTED`
//!   or `RUN_ERROR` (LIFE-03). A run is open between `RUN_STARTED` and its
//!   terminal; a second `RUN_STARTED` inside it is a violation (LIFE-44).
//!   After `RUN_FINISHED` only `RUN_STARTED` and a late `RUN_ERROR` are
//!   accepted; after `RUN_ERROR` only `RUN_STARTED` (LIFE-41, LIFE-42). A
//!   `RUN_FINISHED` names the run that `RUN_STARTED` opened (RUNIN-04,
//!   RUNIN-05) and finds nothing open (LIFE-51, LIFE-53). Told the requested
//!   run ([`ConsumerVerifier::expecting`]), every run's boundary events name
//!   the requested thread and the requested run must appear somewhere on the
//!   stream, before, between or after other runs (RUNIN-06). `RUN_ERROR` ends
//!   whatever was open with the run (LIFE-48, SUB-34).
//! * **Items** (`patterns/streaming.mdx`). Text messages, tool calls,
//!   reasoning messages and reasoning spans each have an open set, keyed by
//!   their id; spans and reasoning messages are separate sets (REAS-12). An
//!   opener for an open id, or a content/end for an id that is not open, is a
//!   violation (TEXT-11, TOOL-37, REAS-08..REAS-11). A closed item may be
//!   reopened (TEXT-05, TOOL-06, LIFE-47).
//! * **Steps** are per owner: a parent and a subagent may hold same-named
//!   steps open, and `STEP_FINISHED` must carry its `STEP_STARTED`'s
//!   attribution (SUB-23, LIFE-53).
//! * **Ownership** (`events/subagents.mdx`). A continuation's tag must agree
//!   with its opener's owner; an untagged continuation always agrees (SUB-24,
//!   SUB-25). A reopening start must name the same owner. A tool call tagged
//!   to one owner may not hang off a message of another; an untagged call
//!   inherits its message's owner (SUB-26), text or reasoning alike. Activity deltas agree with the snapshot
//!   that opened the activity message (SUB-21).
//! * **Subagents.** Attribution without announcement is accepted (SUB-12). An
//!   announcement of an active or already-finished id, or naming an
//!   unannounced parent, is a violation; so is a terminal for an invocation
//!   that is not active (SUB-36), and a `RUN_FINISHED` with one active.
//!
//! Run-scoped tracking resets at every `RUN_STARTED` (LIFE-48).
//!
//! # Cost
//!
//! Every check is a hash lookup keyed by an id (an `Arc<str>` newtype, so
//! inserting one is a reference-count increment). Maps are cleared, not
//! dropped, between runs, so their capacity is reused, unless a table is far
//! larger than the run that just used it (clearing costs O(capacity)). Content events
//! allocate nothing.
//!
//! The verifier keeps one entry per id the stream names: each open step,
//! each item and subagent invocation of the run, and every message id the
//! thread has used. It keeps an estimate of what those hold
//! (docs/DECISIONS.md D25), which a [`Processor`](crate::pipeline::Processor)
//! charges to [`Limits::max_history_bytes`](crate::pipeline::Limits::max_history_bytes);
//! a verifier used alone is bounded only by the stream its host feeds it.

use std::collections::{HashMap, HashSet};
use std::hash::Hash;

use crate::diagnostics::shown;
use crate::error::{Error, ErrorCode, Result};
use crate::event::Event;
use crate::ids::{MessageId, RunId, SubagentRunId, ThreadId, ToolCallId};
use crate::json::text_bytes;
use crate::pipeline::chunks::LANE_SHARE;
use crate::pipeline::{id_heap, owner_heap, table_slot};

/// Who owns an item: `None` is the parent agent.
type Owner = Option<SubagentRunId>;

/// Step names kept across runs before the table is cleared instead.
const MAX_RETAINED_STEP_NAMES: usize = 64;

// Estimated resident bytes of the verifier's entries (D25). Ids are
// `Arc<str>` (16 bytes); the id's own heap is charged once, by the table
// that keeps it for longest (`known_messages` for message ids).

/// An entry of a set of ids.
const ID_SLOT: usize = table_slot(16);
/// A tracker entry, without its id's heap.
const ENTRY_SLOT: usize = table_slot(16 + std::mem::size_of::<Entry>());
/// A step name's entry, without the name's text: the slot and the owner
/// set's own smallest table (four buckets and a control group).
const STEP_NAME_SLOT: usize = table_slot(24 + std::mem::size_of::<HashSet<Owner>>()) + 80;
/// An activity message's entry, without its id's heap.
const ACTIVITY_SLOT: usize = table_slot(16 + std::mem::size_of::<Owner>());

/// What a step name's entry is charged, its owners aside.
fn step_name_bytes(name: &str) -> usize {
    STEP_NAME_SLOT + text_bytes(name.len())
}

/// What an owner in a step's owner set is charged.
fn step_owner_bytes(owner: &Owner) -> usize {
    ID_SLOT + owner_heap(owner.as_ref())
}

/// Where the stream is in the run lifecycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum RunPhase {
    /// No run has started yet.
    Idle,
    /// A run is open.
    Running,
    /// The last run finished (`RUN_FINISHED`).
    Finished,
    /// The last run failed (`RUN_ERROR`).
    Failed,
}

/// The consumer-side verifier.
#[derive(Debug, Default)]
pub struct ConsumerVerifier {
    state: VerifierState,
}

#[derive(Debug)]
struct VerifierState {
    phase: RunPhase,
    /// Identity of the open run, from its `RUN_STARTED`.
    run: Option<(ThreadId, RunId)>,
    text: Tracker<MessageId>,
    tools: Tracker<ToolCallId>,
    reasoning: Tracker<MessageId>,
    spans: Tracker<MessageId>,
    /// Step name → owners holding a step of that name open.
    steps: HashMap<String, HashSet<Owner>>,
    open_steps: usize,
    /// Activity message → owner of the snapshot that opened it.
    activity: HashMap<MessageId, Owner>,
    /// Invocations announced and not yet terminated.
    subagents_active: HashSet<SubagentRunId>,
    /// Invocations announced in this run, active or not.
    subagents_announced: HashSet<SubagentRunId>,
    /// The requested run, when the verifier was told it ([`ConsumerVerifier::expecting`]).
    expected: Option<(ThreadId, RunId)>,
    /// Whether the requested run has appeared on this stream: a
    /// `RUN_STARTED` named it, or the stream opened with a `RUN_ERROR`
    /// (the requested run failed before it started, LIFE-03).
    requested_seen: bool,
    /// Every message id the thread holds: the history given to
    /// [`ConsumerVerifier::with_known_messages`] plus each message this
    /// stream has opened or minted. Thread-wide, never reset (BASIC-15).
    known_messages: HashSet<MessageId>,
    /// Estimated bytes of the run-scoped tables other than the trackers
    /// (step owners, activity owners, subagent sets), freed by `reset_run`.
    run_bytes: usize,
    /// Estimated bytes of the step names held, which outlive a run.
    step_name_bytes: usize,
    /// Estimated bytes of the ids this stream added to `known_messages`;
    /// never credited (BASIC-15 keeps them). Seeds are not counted.
    known_bytes: usize,
}

impl Default for VerifierState {
    fn default() -> Self {
        VerifierState {
            phase: RunPhase::Idle,
            run: None,
            text: Tracker::charging(LANE_SHARE),
            tools: Tracker::charging(LANE_SHARE),
            reasoning: Tracker::charging(LANE_SHARE),
            spans: Tracker::default(),
            steps: HashMap::new(),
            open_steps: 0,
            activity: HashMap::new(),
            subagents_active: HashSet::new(),
            subagents_announced: HashSet::new(),
            expected: None,
            requested_seen: false,
            known_messages: HashSet::new(),
            run_bytes: 0,
            step_name_bytes: 0,
            known_bytes: 0,
        }
    }
}

/// Items of one kind seen in the current run, with their owner and whether
/// they are open.
#[derive(Debug)]
struct Tracker<K> {
    items: HashMap<K, Entry>,
    open: usize,
    /// Estimated bytes the entries hold (D25), freed by `reset`.
    bytes: usize,
    /// Charged per subagent-owned entry beyond its slot and owner: a chunk
    /// lane's share.
    extra: usize,
    /// The open item the last non-closing continuation found, as `items`'
    /// own key with its owner: a run of deltas to one item hashes its id
    /// once. Only set for an open item; forgotten by every `start`, close
    /// and `reset`, the only changes `items` sees.
    hot: Option<(K, Owner)>,
}

impl<K> Default for Tracker<K> {
    fn default() -> Self {
        Tracker::charging(0)
    }
}

impl<K> Tracker<K> {
    fn charging(extra: usize) -> Self {
        Tracker {
            items: HashMap::new(),
            open: 0,
            bytes: 0,
            extra,
            hot: None,
        }
    }
}

#[derive(Debug)]
struct Entry {
    owner: Owner,
    open: bool,
}

/// Why a tracker refused an event.
enum Refusal {
    AlreadyOpen,
    NotOpen,
    /// The owner the item already has.
    Owner(Owner),
}

impl<K: Hash + Eq + Clone + AsRef<str>> Tracker<K> {
    fn reset(&mut self) {
        crate::pipeline::reset_map(&mut self.items);
        self.open = 0;
        self.bytes = 0;
        self.hot = None;
    }

    fn owner_of(&self, id: &K) -> Option<&Owner> {
        self.items.get(id).map(|entry| &entry.owner)
    }

    /// Opens `id` under `owner`; a closed item reopens under the same owner.
    /// A new entry is charged `id_bytes` for the id's heap (zero when another
    /// table keeps the same `Arc` and charges it).
    fn start(
        &mut self,
        id: &K,
        owner: &Owner,
        id_bytes: usize,
    ) -> std::result::Result<(), Refusal> {
        self.hot = None;
        match self.items.get_mut(id) {
            Some(entry) if entry.open => Err(Refusal::AlreadyOpen),
            Some(entry) if entry.owner != *owner => Err(Refusal::Owner(entry.owner.clone())),
            Some(entry) => {
                entry.open = true;
                self.open += 1;
                Ok(())
            }
            None => {
                self.items.insert(
                    id.clone(),
                    Entry {
                        owner: owner.clone(),
                        open: true,
                    },
                );
                self.open += 1;
                // A subagent's chunk lane is keyed by its owner; the
                // parent's is a single slot.
                let lane = if owner.is_some() { self.extra } else { 0 };
                self.bytes = self
                    .bytes
                    .saturating_add(ENTRY_SLOT + lane + id_bytes + owner_heap(owner.as_ref()));
                Ok(())
            }
        }
    }

    /// Checks a continuation (and closes the item when `close`).
    fn touch(
        &mut self,
        id: &K,
        tag: Option<&SubagentRunId>,
        close: bool,
    ) -> std::result::Result<(), Refusal> {
        if !close {
            if let Some((hot, owner)) = &self.hot {
                if hot == id && tag.map_or(true, |tag| owner.as_ref() == Some(tag)) {
                    debug_assert!(
                        self.items
                            .get(id)
                            .is_some_and(|e| e.open && e.owner == *owner),
                        "stale hot item"
                    );
                    return Ok(());
                }
            }
        }
        self.hot = None;
        let Some(entry) = self.items.get_mut(id) else {
            return Err(Refusal::NotOpen);
        };
        if !entry.open {
            return Err(Refusal::NotOpen);
        }
        if let Some(tag) = tag {
            if entry.owner.as_ref() != Some(tag) {
                return Err(Refusal::Owner(entry.owner.clone()));
            }
        }
        if close {
            entry.open = false;
            self.open -= 1;
        } else {
            // `items`' own key (looked up once more, on the first delta of a
            // run only): the cache keeps no id alive that the table does not
            // hold and charge already (D25).
            let owner = entry.owner.clone();
            self.hot = self
                .items
                .get_key_value(id)
                .map(|(key, _)| (key.clone(), owner));
        }
        Ok(())
    }

    /// The open ids, sorted, quoted and comma-separated (error path only).
    fn open_list(&self) -> String {
        let mut ids: Vec<&str> = self
            .items
            .iter()
            .filter(|(_, entry)| entry.open)
            .map(|(id, _)| id.as_ref())
            .collect();
        ids.sort_unstable();
        quote_list(ids)
    }
}

/// Most ids quoted by [`quote_list`]; the rest are counted.
pub(super) const MAX_LISTED_IDS: usize = 16;

/// Single-quotes each id (previewed) and joins up to [`MAX_LISTED_IDS`] with
/// commas, then counts the rest, for error text.
pub(super) fn quote_list<I>(ids: I) -> String
where
    I: IntoIterator,
    I::Item: AsRef<str>,
{
    let mut quoted = Vec::new();
    let mut more = 0_usize;
    for id in ids {
        if quoted.len() < MAX_LISTED_IDS {
            quoted.push(format!("'{}'", shown(id.as_ref())));
        } else {
            more += 1;
        }
    }
    let mut text = quoted.join(", ");
    if more > 0 {
        text.push_str(&format!(", and {more} more"));
    }
    text
}

fn owner_text(owner: &Owner) -> String {
    match owner {
        Some(id) => format!("subagent '{}'", shown(id.as_str())),
        None => "the parent agent".to_owned(),
    }
}

/// Item families, for messages.
#[derive(Clone, Copy)]
enum Family {
    Text,
    Tool,
    Reasoning,
    Span,
}

impl Family {
    const fn code(self) -> ErrorCode {
        match self {
            Family::Text => ErrorCode::TextMessage,
            Family::Tool => ErrorCode::ToolCall,
            Family::Reasoning | Family::Span => ErrorCode::Reasoning,
        }
    }

    const fn noun(self) -> &'static str {
        match self {
            Family::Text => "text message",
            Family::Tool => "tool call",
            Family::Reasoning => "reasoning message",
            Family::Span => "reasoning span",
        }
    }

    const fn plural(self) -> &'static str {
        match self {
            Family::Text => "text messages",
            Family::Tool => "tool calls",
            Family::Reasoning => "reasoning messages",
            Family::Span => "reasoning spans",
        }
    }
}

fn refusal_error(
    family: Family,
    event_type: &str,
    id: &str,
    refusal: Refusal,
    tag: &Owner,
) -> Error {
    let id = &*shown(id);
    match refusal {
        Refusal::AlreadyOpen => Error::new(
            family.code(),
            format!(
                "Cannot send '{event_type}' event: A {} with ID '{id}' is already in progress. \
                 Complete it before starting another with the same ID.",
                family.noun()
            ),
        ),
        Refusal::NotOpen => Error::new(
            family.code(),
            format!(
                "Cannot send '{event_type}' event: No active {} found with ID '{id}'. \
                 Start it first.",
                family.noun()
            ),
        ),
        Refusal::Owner(owner) => Error::new(
            ErrorCode::Ownership,
            format!(
                "Cannot send '{event_type}' event for {} '{id}': its attribution ({}) does not \
                 match the {}'s owner ({})",
                family.noun(),
                owner_text(tag),
                family.noun(),
                owner_text(&owner)
            ),
        ),
    }
}

fn lifecycle_error(message: String) -> Error {
    Error::new(ErrorCode::RunLifecycle, message)
}

impl ConsumerVerifier {
    /// A verifier for one stream, checking stream-local rules only.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// A verifier that also checks identity: the requested run is `run_id`
    /// on `thread_id` (RUNIN-04, RUNIN-05; `verify.identity`).
    ///
    /// Every run's `RUN_STARTED` and `RUN_FINISHED` must name `thread_id`
    /// (RUNIN-04), and each run's two boundary events agree on `runId`
    /// (RUNIN-05). Runs with other ids (replayed history, or a run the agent
    /// started after a failure) may come before or after the requested run;
    /// the requested run itself need not be the last. It must appear,
    /// though: a stream that ends having run only other ids fails
    /// [`ConsumerVerifier::finish`] with `verify.identity` (RUNIN-06).
    #[must_use]
    pub fn expecting(thread_id: ThreadId, run_id: RunId) -> Self {
        let mut verifier = Self::default();
        verifier.set_expected(thread_id, run_id);
        verifier
    }

    pub(crate) fn set_expected(&mut self, thread_id: ThreadId, run_id: RunId) {
        self.state.expected = Some((thread_id, run_id));
    }

    /// Estimated resident bytes of what the verifier keeps under the ids
    /// this stream named (D25): what a [`Processor`](crate::pipeline::Processor)
    /// charges to the history budget. Ids given to
    /// [`with_known_messages`](Self::with_known_messages) are not counted.
    pub(crate) fn retained(&self) -> usize {
        let s = &self.state;
        [
            s.text.bytes,
            s.tools.bytes,
            s.reasoning.bytes,
            s.spans.bytes,
            s.run_bytes,
            s.step_name_bytes,
            s.known_bytes,
        ]
        .iter()
        .fold(0_usize, |sum, b| sum.saturating_add(*b))
    }

    /// Message ids already in the thread's history, so a stream reusing one
    /// for a new message is caught (BASIC-15).
    ///
    /// Restreaming a held text or reasoning message appends to it (LIFE-47),
    /// so a start naming a known id is a reopen, not a reuse. What a stream
    /// cannot do is mint a *new* message under a held id: a `TOOL_CALL_RESULT`
    /// whose `messageId` the thread already holds is refused. The stream's
    /// own messages join the set as they appear.
    #[must_use]
    pub fn with_known_messages(mut self, ids: impl IntoIterator<Item = MessageId>) -> Self {
        self.state.known_messages.extend(ids);
        self
    }

    /// Checks one expanded event.
    ///
    /// A refused event leaves the verifier where it was, but the stream it
    /// came from has failed: the caller must not continue it.
    ///
    /// # Errors
    ///
    /// The `verify.*` code of the violated rule family.
    pub fn observe(&mut self, event: &Event) -> Result<()> {
        let state = &mut self.state;
        let event_type = event.event_type().as_str();
        match state.phase {
            RunPhase::Idle => {
                if !matches!(event, Event::RunStarted(_) | Event::RunError(_)) {
                    return Err(lifecycle_error(format!(
                        "First event must be 'RUN_STARTED' or 'RUN_ERROR', got '{event_type}'"
                    )));
                }
            }
            RunPhase::Finished => {
                if !matches!(event, Event::RunStarted(_) | Event::RunError(_)) {
                    return Err(lifecycle_error(format!(
                        "Cannot send '{event_type}' event: The run has already finished with \
                         'RUN_FINISHED'. Start a new run with 'RUN_STARTED'."
                    )));
                }
            }
            RunPhase::Failed => {
                if !matches!(event, Event::RunStarted(_)) {
                    return Err(lifecycle_error(format!(
                        "Cannot send '{event_type}' event: The run has already errored with \
                         'RUN_ERROR'. Start a new run with 'RUN_STARTED'."
                    )));
                }
            }
            RunPhase::Running => {}
        }
        state.check(event, event_type)
    }

    /// Ends the stream's identity check (RUNIN-06): with a requested run
    /// ([`expecting`](Self::expecting)) and a stream whose last run reached
    /// its terminal, the requested run must have appeared. A stream that
    /// ended inside a run is truncated rather than misidentified, and is not
    /// judged here; neither is a verifier with no requested run.
    ///
    /// # Errors
    ///
    /// `verify.identity` when every run on a completed stream named another
    /// run id.
    pub fn finish(&self) -> Result<()> {
        let state = &self.state;
        match &state.expected {
            Some((_, run_id))
                if !state.requested_seen
                    && matches!(state.phase, RunPhase::Finished | RunPhase::Failed) =>
            {
                Err(Error::new(
                    ErrorCode::Identity,
                    format!(
                        "the stream ended without the requested run '{}': every run on it \
                         named another runId",
                        shown(run_id.as_str())
                    ),
                ))
            }
            _ => Ok(()),
        }
    }

    /// Current lifecycle phase. [`RunPhase::Running`] at end of stream means
    /// the stream was truncated (TRN-09, SSE-15).
    #[must_use]
    pub fn phase(&self) -> RunPhase {
        self.state.phase
    }
}

impl VerifierState {
    fn reset_run(&mut self) {
        self.run = None;
        self.text.reset();
        self.tools.reset();
        self.reasoning.reset();
        self.spans.reset();
        // Step names recur run after run; keep their entries (and owner
        // lists) so reopening one allocates nothing, unless the set of names
        // has grown large.
        if self.steps.len() > MAX_RETAINED_STEP_NAMES
            || self.steps.capacity() > crate::pipeline::RETAIN_FLOOR
        {
            self.steps = HashMap::new();
            self.step_name_bytes = 0;
        } else {
            self.steps.values_mut().for_each(crate::pipeline::reset_set);
        }
        self.open_steps = 0;
        crate::pipeline::reset_map(&mut self.activity);
        crate::pipeline::reset_set(&mut self.subagents_active);
        crate::pipeline::reset_set(&mut self.subagents_announced);
        // Step owners, activity owners and subagent sets are all gone.
        self.run_bytes = 0;
    }

    /// The `Arc` `known_messages` keeps for `id`, if any, so a table keyed by
    /// the same message shares it instead of pinning the event's copy.
    fn interned(&self, id: &MessageId) -> MessageId {
        self.known_messages.get(id).unwrap_or(id).clone()
    }

    /// Adds `id` to the thread's message ids, charging it when new.
    fn know(&mut self, id: &MessageId) {
        if !self.known_messages.contains(id) {
            self.known_messages.insert(id.clone());
            self.known_bytes = self
                .known_bytes
                .saturating_add(ID_SLOT + id_heap(id.as_str().len()));
        }
    }

    /// Checks an event the lifecycle gate admitted.
    fn check(&mut self, event: &Event, event_type: &str) -> Result<()> {
        match event {
            Event::RunStarted(e) => {
                if self.phase == RunPhase::Running {
                    return Err(lifecycle_error(
                        "Cannot send 'RUN_STARTED' while a run is still active. The current run \
                         must end with 'RUN_FINISHED' or 'RUN_ERROR' first."
                            .to_owned(),
                    ));
                }
                if let Some((thread_id, run_id)) = &self.expected {
                    if *thread_id != e.thread_id {
                        return Err(Error::new(
                            ErrorCode::Identity,
                            format!(
                                "'RUN_STARTED' names thread '{}', but the request was for \
                                 thread '{}'",
                                shown(e.thread_id.as_str()),
                                shown(thread_id.as_str())
                            ),
                        ));
                    }
                    if *run_id == e.run_id {
                        self.requested_seen = true;
                    }
                }
                self.reset_run();
                self.run = Some((e.thread_id.clone(), e.run_id.clone()));
                self.phase = RunPhase::Running;
                Ok(())
            }
            Event::RunFinished(e) => {
                if let Some((thread_id, run_id)) = &self.run {
                    if *run_id != e.run_id || *thread_id != e.thread_id {
                        return Err(Error::new(
                            ErrorCode::Identity,
                            format!(
                                "'RUN_FINISHED' names thread '{}' and run '{}', which does not \
                                 match the active run (thread '{}', run '{}')",
                                shown(e.thread_id.as_str()),
                                shown(e.run_id.as_str()),
                                shown(thread_id.as_str()),
                                shown(run_id.as_str())
                            ),
                        ));
                    }
                }
                self.check_nothing_open()?;
                self.reset_run();
                self.phase = RunPhase::Finished;
                Ok(())
            }
            Event::RunError(_) => {
                // A stream that opens with RUN_ERROR is the requested run
                // failing before it started (LIFE-03): it carries no ids.
                if self.phase == RunPhase::Idle {
                    self.requested_seen = true;
                }
                // Abandons everything still open (LIFE-48, SUB-34).
                self.reset_run();
                self.phase = RunPhase::Failed;
                Ok(())
            }
            Event::StepStarted(e) => {
                let owner = e.subagent_run_id.clone();
                if let Some(owners) = self.steps.get_mut(e.step_name.as_str()) {
                    // Indexed by owner: a stream of distinct owners costs
                    // O(1) each, never a scan of the open ones.
                    if owners.contains(&owner) {
                        return Err(Error::new(
                            ErrorCode::Step,
                            format!(
                                "Cannot send 'STEP_STARTED' event: Step '{}' is already active \
                                 for {}",
                                shown(&e.step_name),
                                owner_text(&owner)
                            ),
                        ));
                    }
                    self.run_bytes = self.run_bytes.saturating_add(step_owner_bytes(&owner));
                    owners.insert(owner);
                } else {
                    self.step_name_bytes = self
                        .step_name_bytes
                        .saturating_add(step_name_bytes(&e.step_name));
                    self.run_bytes = self.run_bytes.saturating_add(step_owner_bytes(&owner));
                    self.steps
                        .insert(e.step_name.clone(), HashSet::from([owner]));
                }
                self.open_steps += 1;
                Ok(())
            }
            Event::StepFinished(e) => {
                let removed = self
                    .steps
                    .get_mut(e.step_name.as_str())
                    .map(|owners| (owners.remove(&e.subagent_run_id), owners.is_empty()));
                if let Some((true, now_empty)) = removed {
                    self.open_steps -= 1;
                    self.run_bytes = self
                        .run_bytes
                        .saturating_sub(step_owner_bytes(&e.subagent_run_id));
                    // Names recur, so entries are kept up to the cap; beyond
                    // it a finished name is forgotten (an empty owner set and
                    // an absent key both mean "not started").
                    if now_empty && self.steps.len() > MAX_RETAINED_STEP_NAMES {
                        self.steps.remove(e.step_name.as_str());
                        self.step_name_bytes = self
                            .step_name_bytes
                            .saturating_sub(step_name_bytes(&e.step_name));
                    }
                    Ok(())
                } else {
                    Err(Error::new(
                        ErrorCode::Step,
                        format!(
                            "Cannot send 'STEP_FINISHED' for step '{}' that was not started by \
                             {}",
                            shown(&e.step_name),
                            owner_text(&e.subagent_run_id)
                        ),
                    ))
                }
            }
            Event::TextMessageStart(e) => {
                // The id's heap is charged by `known_messages`, which keeps
                // the same `Arc`.
                let id = self.interned(&e.message_id);
                self.text.start(&id, &e.subagent_run_id, 0).map_err(|r| {
                    refusal_error(
                        Family::Text,
                        event_type,
                        e.message_id.as_str(),
                        r,
                        &e.subagent_run_id,
                    )
                })?;
                self.know(&id);
                Ok(())
            }
            Event::TextMessageContent(e) => touch(
                &mut self.text,
                Family::Text,
                event_type,
                &e.message_id,
                &e.subagent_run_id,
                false,
            ),
            Event::TextMessageEnd(e) => touch(
                &mut self.text,
                Family::Text,
                event_type,
                &e.message_id,
                &e.subagent_run_id,
                true,
            ),
            Event::ToolCallStart(e) => {
                let mut owner = e.subagent_run_id.clone();
                if let Some(parent) = &e.parent_message_id {
                    // The parent is whichever message the producer verifier
                    // would find: a text message, else a reasoning message.
                    let message_owner = self
                        .text
                        .owner_of(parent)
                        .or_else(|| self.reasoning.owner_of(parent));
                    if let Some(message_owner) = message_owner {
                        match &owner {
                            Some(_) if owner != *message_owner => {
                                return Err(Error::new(
                                    ErrorCode::Ownership,
                                    format!(
                                        "Cannot send 'TOOL_CALL_START' event for tool call '{}': \
                                         its attribution ({}) does not match the owner of its \
                                         parent message '{}' ({})",
                                        shown(e.tool_call_id.as_str()),
                                        owner_text(&owner),
                                        shown(parent.as_str()),
                                        owner_text(message_owner)
                                    ),
                                ));
                            }
                            Some(_) => {}
                            // An untagged call inherits its message's owner.
                            None => owner = message_owner.clone(),
                        }
                    }
                }
                self.tools
                    .start(
                        &e.tool_call_id,
                        &owner,
                        id_heap(e.tool_call_id.as_str().len()),
                    )
                    .map_err(|r| {
                        refusal_error(Family::Tool, event_type, e.tool_call_id.as_str(), r, &owner)
                    })?;
                // The reducer holds a message under the named parent (minting
                // it when absent), so a result may not reuse the id (BASIC-15).
                if let Some(parent) = &e.parent_message_id {
                    self.know(parent);
                }
                Ok(())
            }
            Event::ToolCallArgs(e) => touch(
                &mut self.tools,
                Family::Tool,
                event_type,
                &e.tool_call_id,
                &e.subagent_run_id,
                false,
            ),
            Event::ToolCallEnd(e) => touch(
                &mut self.tools,
                Family::Tool,
                event_type,
                &e.tool_call_id,
                &e.subagent_run_id,
                true,
            ),
            Event::ReasoningStart(e) => self
                .spans
                .start(
                    &e.message_id,
                    &e.subagent_run_id,
                    id_heap(e.message_id.as_str().len()),
                )
                .map_err(|r| {
                    refusal_error(
                        Family::Span,
                        event_type,
                        e.message_id.as_str(),
                        r,
                        &e.subagent_run_id,
                    )
                }),
            Event::ReasoningEnd(e) => touch(
                &mut self.spans,
                Family::Span,
                event_type,
                &e.message_id,
                &e.subagent_run_id,
                true,
            ),
            Event::ReasoningMessageStart(e) => {
                let id = self.interned(&e.message_id);
                self.reasoning
                    .start(&id, &e.subagent_run_id, 0)
                    .map_err(|r| {
                        refusal_error(
                            Family::Reasoning,
                            event_type,
                            e.message_id.as_str(),
                            r,
                            &e.subagent_run_id,
                        )
                    })?;
                self.know(&id);
                Ok(())
            }
            Event::ReasoningMessageContent(e) => touch(
                &mut self.reasoning,
                Family::Reasoning,
                event_type,
                &e.message_id,
                &e.subagent_run_id,
                false,
            ),
            Event::ReasoningMessageEnd(e) => touch(
                &mut self.reasoning,
                Family::Reasoning,
                event_type,
                &e.message_id,
                &e.subagent_run_id,
                true,
            ),
            Event::ActivitySnapshot(e) => {
                let keeps_owner =
                    e.replace == Some(false) && self.activity.contains_key(&e.message_id);
                if !keeps_owner {
                    let id = self.interned(&e.message_id);
                    let charge = owner_heap(e.subagent_run_id.as_ref());
                    let (charge, credit) =
                        match self.activity.insert(id.clone(), e.subagent_run_id.clone()) {
                            // The key stays; only the owner was replaced.
                            Some(old) => (charge, owner_heap(old.as_ref())),
                            None => (ACTIVITY_SLOT + charge, 0),
                        };
                    self.run_bytes = self.run_bytes.saturating_add(charge).saturating_sub(credit);
                    self.know(&id);
                } else {
                    self.know(&e.message_id);
                }
                Ok(())
            }
            Event::ActivityDelta(e) => {
                if let (Some(tag), Some(owner)) =
                    (e.subagent_run_id.as_ref(), self.activity.get(&e.message_id))
                {
                    if owner.as_ref() != Some(tag) {
                        return Err(Error::new(
                            ErrorCode::Ownership,
                            format!(
                                "Cannot send 'ACTIVITY_DELTA' event for activity message '{}': \
                                 its attribution ({}) does not match the activity message's \
                                 owner ({})",
                                shown(e.message_id.as_str()),
                                owner_text(&e.subagent_run_id),
                                owner_text(owner)
                            ),
                        ));
                    }
                }
                Ok(())
            }
            Event::SubagentStarted(e) => {
                let id = &e.subagent_run_id;
                if self.subagents_active.contains(id) {
                    return Err(Error::new(
                        ErrorCode::Subagent,
                        format!(
                            "Cannot send 'SUBAGENT_STARTED' event: subagent '{}' is already active",
                            shown(id.as_str())
                        ),
                    ));
                }
                if self.subagents_announced.contains(id) {
                    return Err(Error::new(
                        ErrorCode::Subagent,
                        format!(
                            "Cannot send 'SUBAGENT_STARTED' event: subagent '{}' already finished \
                             in this run; a new invocation needs a new subagentRunId",
                            shown(id.as_str())
                        ),
                    ));
                }
                if let Some(parent) = &e.parent_subagent_run_id {
                    if !self.subagents_announced.contains(parent) {
                        return Err(Error::new(
                            ErrorCode::Subagent,
                            format!(
                                "Cannot send 'SUBAGENT_STARTED' event for subagent '{}': its \
                                 parentSubagentRunId '{}' has not been announced in this run",
                                shown(id.as_str()),
                                shown(parent.as_str())
                            ),
                        ));
                    }
                }
                self.subagents_active.insert(id.clone());
                self.subagents_announced.insert(id.clone());
                // One `Arc` in both sets: its heap is charged once.
                self.run_bytes = self
                    .run_bytes
                    .saturating_add(2 * ID_SLOT + id_heap(id.as_str().len()));
                Ok(())
            }
            Event::SubagentFinished(e) => self.end_subagent(event_type, &e.subagent_run_id),
            Event::SubagentError(e) => self.end_subagent(event_type, &e.subagent_run_id),
            Event::TextMessageChunk(_)
            | Event::ToolCallChunk(_)
            | Event::ReasoningMessageChunk(_) => Err(Error::new(
                ErrorCode::Chunk,
                format!("'{event_type}' must be expanded before verification"),
            )),
            Event::ToolCallResult(e) => {
                if self.known_messages.contains(&e.message_id) {
                    return Err(Error::new(
                        ErrorCode::Identity,
                        format!(
                            "Cannot send 'TOOL_CALL_RESULT' event: messageId '{}' already names \
                             a message in this thread",
                            shown(e.message_id.as_str())
                        ),
                    ));
                }
                self.know(&e.message_id);
                Ok(())
            }
            Event::MessagesSnapshot(e) => {
                // The snapshot's messages are the thread's from now on: a
                // later TOOL_CALL_RESULT may not mint under one of their ids
                // (BASIC-15). A message the snapshot leaves out keeps its id
                // reserved, as it was already used in this thread.
                for message in &e.messages {
                    self.know(message.id());
                }
                Ok(())
            }
            _ => Ok(()),
        }
    }

    fn end_subagent(&mut self, event_type: &str, id: &SubagentRunId) -> Result<()> {
        if self.subagents_active.remove(id) {
            self.run_bytes = self.run_bytes.saturating_sub(ID_SLOT);
            Ok(())
        } else {
            Err(Error::new(
                ErrorCode::Subagent,
                format!(
                    "Cannot send '{event_type}' for subagent '{}' that was not started (no \
                     active 'SUBAGENT_STARTED' with that subagentRunId)",
                    shown(id.as_str())
                ),
            ))
        }
    }

    /// `RUN_FINISHED` requires everything the run opened to be closed.
    fn check_nothing_open(&self) -> Result<()> {
        let open = |family: Family, list: String| {
            Error::new(
                family.code(),
                format!(
                    "Cannot send 'RUN_FINISHED' while {} are still active: {list}",
                    family.plural()
                ),
            )
        };
        if self.text.open > 0 {
            return Err(open(Family::Text, self.text.open_list()));
        }
        if self.tools.open > 0 {
            return Err(open(Family::Tool, self.tools.open_list()));
        }
        if self.reasoning.open > 0 {
            return Err(open(Family::Reasoning, self.reasoning.open_list()));
        }
        if self.spans.open > 0 {
            return Err(open(Family::Span, self.spans.open_list()));
        }
        if self.open_steps > 0 {
            let mut names: Vec<&str> = self
                .steps
                .iter()
                .filter(|(_, owners)| !owners.is_empty())
                .map(|(name, _)| name.as_str())
                .collect();
            names.sort_unstable();
            return Err(Error::new(
                ErrorCode::Step,
                format!(
                    "Cannot send 'RUN_FINISHED' while steps are still active: {}",
                    quote_list(names)
                ),
            ));
        }
        if !self.subagents_active.is_empty() {
            let mut ids: Vec<&str> = self.subagents_active.iter().map(|id| id.as_str()).collect();
            ids.sort_unstable();
            return Err(Error::new(
                ErrorCode::Subagent,
                format!(
                    "Cannot send 'RUN_FINISHED' while subagents are still active: {}",
                    quote_list(ids)
                ),
            ));
        }
        Ok(())
    }
}

fn touch<K: Hash + Eq + Clone + AsRef<str>>(
    tracker: &mut Tracker<K>,
    family: Family,
    event_type: &str,
    id: &K,
    tag: &Owner,
    close: bool,
) -> Result<()> {
    tracker
        .touch(id, tag.as_ref(), close)
        .map_err(|r| refusal_error(family, event_type, id.as_ref(), r, tag))
}

#[cfg(test)]
mod tests;
