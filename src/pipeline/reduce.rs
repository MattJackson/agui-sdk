//! The reducer: verified events → message history and state.
//!
//! Applies the spec's application rules: deltas concatenate (STRM-04);
//! metadata merges key by key into the item an event builds (META-06..META-13);
//! `STATE_SNAPSHOT` replaces and `STATE_DELTA` patches atomically, a failed
//! patch warns and keeps the prior state (SNAP-01..SNAP-16, STATE-01..STATE-03);
//! `MESSAGES_SNAPSHOT` reconciles by id, keeps position, appends new ones and
//! drops absent ones except client-only roles (STATE-04..STATE-08); activity
//! snapshots and deltas follow ACT-02..ACT-16; encrypted values attach to the
//! entity they name (REAS-15, REAS-26); tool results mint tool messages
//! (TOOL-11, TOOL-14); pending frontend calls are derived from the stream,
//! not from `pendingToolCallIds` (TOOL-32).
//!
//! A carrier minted under a parentless call's id holds an id the producer
//! never chose for a message. When the producer later uses that id for a
//! message of its own (a text or reasoning start, a tool result, an activity
//! snapshot, a chunk), the carrier gives the id up: it is renamed to a fresh
//! `<id>~<n>` and keeps its position and calls, and the producer's message
//! is new, so a conformant stream's message never merges into the carrier.
//! [`Reducer::run_message_ids`] follows the rename. A carrier the producer
//! adopts (names as a later call's parent) or a snapshot replaces is the
//! producer's message from then on. Renames are per reducer: a carrier in
//! history a processor starts from is treated as the producer's.
//!
//! # Cost
//!
//! History is a `Vec<Message>` plus an id → index map: a content delta is an
//! O(1) lookup and an amortized O(delta) string append, and the history is
//! never cloned per event. Callers borrow the history ([`Reducer::messages`])
//! between events instead of receiving copies.
//!
//! [`Reducer::apply`] takes the event **by value** and returns it as an
//! `Arc<Event>` inside [`Processed`](super::Processed), shared with the
//! application. A `STATE_SNAPSHOT` is installed without a copy: the state is
//! read through the shared event, and the first later change takes it over
//! with `Arc::try_unwrap` (no copy once the application has dropped the
//! update; a copy-on-write copy only if it still holds it). Payloads that
//! become part of the history (`MESSAGES_SNAPSHOT` messages, tool result
//! content, activity content, metadata) and operation values patched into a
//! document are copied once, because [`Reducer::messages`] lends `&[Message]`
//! while the shared event keeps its own payload for the application.
//!
//! The `Arc` itself is recycled on the streaming hot path. After a content
//! delta (`TEXT_MESSAGE_CONTENT`, `REASONING_MESSAGE_CONTENT`,
//! `TOOL_CALL_ARGS` and their chunk forms) the reducer keeps one extra handle
//! to it; when the next event arrives and the application has dropped its
//! handle, the next event is moved into that allocation instead of a new
//! one. Streaming a message therefore costs no allocation per delta beyond
//! the text's amortized growth. If the application still holds the update,
//! a fresh `Arc` is made, as for every other event. Events that carry a
//! payload (snapshots, results, custom and raw events) are never retained,
//! so the application's handle to them stays unique and
//! `Arc::try_unwrap` on it succeeds once the reducer has let go.

//!
//! # Choices the spec leaves to the consumer
//!
//! * **Tool-call placement.** A call joins the assistant message its
//!   `parentMessageId` names, minting that message (no content) when it is not
//!   held. Without a parent, it joins the latest message when that is an
//!   assistant message of the same owner added by the current run; otherwise a
//!   tool-call-only assistant message is minted under the call's own id
//!   (renamed if the producer later uses that id, above).
//! * **Reopens.** A reopening `TEXT_MESSAGE_START` that disagrees with the
//!   message (role, name or owner) keeps the established values and warns
//!   (TEXT-07). A reopening `TOOL_CALL_START` that disagrees warns; the
//!   latest name wins and the call stays where it is, under its established
//!   parent and owner, as in the TypeScript reference; argument text keeps
//!   appending (TOOL-06, TOOL-08).
//! * **Tool results** are appended in arrival order as a tool message; a
//!   result whose `messageId` is already held **as a tool message answering
//!   the same call** replaces that message in place. Any other held message
//!   with that id is never replaced: the tool message is appended and the id
//!   keeps naming the earlier message, as when a call id clashes with a held
//!   message. A carrier minted under the id gives it up first (above).
//! * **Activity content stays an object** (ACT-15): a delta whose result is
//!   not an object is refused like a failing patch.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::activity::{ActivityDelta, ActivitySnapshot};
use crate::content::{Content, ContentPart, TextPart};
use crate::diagnostics::{Diagnostic, DiagnosticCode, Reporter, Scope};
use crate::event::{Event, EventType};
use crate::ids::{MessageId, SubagentRunId, ToolCallId};
use crate::interrupt::Interrupt;
use crate::json::{map_bytes, resident_bytes, seq_bytes, text_bytes, wire_bytes};
use crate::lifecycle::RunFinishedOutcome;
use crate::message::{
    ActivityMessage, AssistantMessage, DeveloperMessage, Message, ReasoningMessage, SystemMessage,
    ToolMessage, UserMessage,
};
use crate::metadata::Metadata;
use crate::patch::{
    self, CheckedError, PatchBudget, PatchErrorReason, PatchLimits, PatchOperation,
};
use crate::reasoning::EncryptedValueSubtype;
use crate::text::TextMessageRole;
use crate::tool::{FunctionCall, ToolCall, ToolCallType};
use crate::usage::TokenUsage;

/// What applying one event changed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct Changes {
    /// State was replaced or patched.
    pub state: bool,
    /// The message created or updated, if exactly one was.
    pub message: Option<MessageId>,
    /// The history was reconciled wholesale (`MESSAGES_SNAPSHOT`); false
    /// when the snapshot was refused at the reconcile work bound.
    pub messages_replaced: bool,
}

/// A frontend tool call the run left unanswered. Serializable, so a
/// [`ThreadSnapshot`][crate::client::ThreadSnapshot] can persist it.
#[cfg_attr(
    not(feature = "client"),
    doc = "",
    doc = "[crate::client::ThreadSnapshot]: https://docs.rs/agui-sdk/latest/agui_sdk/client/struct.ThreadSnapshot.html"
)]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct PendingToolCall {
    /// The call id.
    pub tool_call_id: ToolCallId,
    /// The tool's name.
    pub name: String,
    /// Complete argument text (the call ended, TOOL-04).
    pub arguments: String,
    /// The assistant message carrying the call.
    pub message_id: MessageId,
}

impl PendingToolCall {
    /// A pending call.
    pub fn new(
        tool_call_id: ToolCallId,
        name: impl Into<String>,
        arguments: impl Into<String>,
        message_id: MessageId,
    ) -> Self {
        PendingToolCall {
            tool_call_id,
            name: name.into(),
            arguments: arguments.into(),
            message_id,
        }
    }
}

/// The state: owned, or still inside the `STATE_SNAPSHOT` event that
/// installed it (shared with the application, taken over on first change).
#[derive(Debug, Clone)]
struct StateSlot {
    /// The state, unless `shared` holds it.
    owned: Value,
    /// The installing `STATE_SNAPSHOT`, while its snapshot is the state.
    shared: Option<Arc<Event>>,
}

impl StateSlot {
    fn owned(value: Value) -> Self {
        StateSlot {
            owned: value,
            shared: None,
        }
    }

    fn shared(event: Arc<Event>) -> Self {
        StateSlot {
            owned: Value::Null,
            shared: Some(event),
        }
    }

    fn get(&self) -> &Value {
        match self.shared.as_deref() {
            Some(Event::StateSnapshot(s)) => &s.snapshot,
            _ => &self.owned,
        }
    }

    /// Owned access: moves the snapshot out of the event when nothing else
    /// holds it, copies it otherwise.
    fn get_mut(&mut self) -> &mut Value {
        if let Some(event) = self.shared.take() {
            self.owned = match Arc::try_unwrap(event) {
                Ok(Event::StateSnapshot(s)) => s.snapshot,
                Ok(_) => Value::Null,
                Err(shared) => match &*shared {
                    Event::StateSnapshot(s) => s.snapshot.clone(),
                    _ => Value::Null,
                },
            };
        }
        &mut self.owned
    }

    fn into_value(mut self) -> Value {
        std::mem::take(self.get_mut())
    }
}

/// The streaming deltas whose `Arc` the reducer recycles: small, frequent,
/// and carrying nothing an application would want to take ownership of.
fn is_content_delta(event: &Event) -> bool {
    matches!(
        event,
        Event::TextMessageContent(_)
            | Event::TextMessageChunk(_)
            | Event::ReasoningMessageContent(_)
            | Event::ReasoningMessageChunk(_)
            | Event::ToolCallArgs(_)
            | Event::ToolCallChunk(_)
    )
}

/// The events after which the remembered delta targets
/// ([`Reducer::message_target`], [`Reducer::args_target`]) still hold: the
/// deltas that only append to a held message or call. Every other event may
/// add, move, replace or drop messages and calls, so it forgets them.
fn keeps_hot_targets(event: &Event) -> bool {
    matches!(
        event,
        Event::TextMessageContent(_) | Event::ReasoningMessageContent(_) | Event::ToolCallArgs(_)
    )
}

/// Where a tool call lives: `messages[message].tool_calls[position]`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct CallSlot {
    message: usize,
    position: usize,
}

/// Tool-call bookkeeping for the current run (TOOL-32).
#[derive(Debug, Clone, Default)]
struct RunCalls {
    /// Calls started (or reopened) by this run, in order, without repeats.
    started: Vec<ToolCallId>,
    started_set: HashSet<ToolCallId>,
    /// Ended and answered calls. A started call's entry shares
    /// `started_set`'s `Arc`, so the event's own copy of the id is freed.
    ended: HashSet<ToolCallId>,
    answered: HashSet<ToolCallId>,
    /// Estimated resident bytes of the four tables (D25), charged to the
    /// history budget and credited when the run's tables are cleared.
    bytes: usize,
}

/// An entry of a set of ids, without the id's heap (D25).
const ID_SLOT: usize = super::table_slot(16);
/// A started call: its `started` slot (with growth slack) and its
/// `started_set` entry, without the id's heap.
const STARTED_SLOT: usize = 32 + ID_SLOT;

/// Whether two ids share one allocation.
fn same_arc(a: &ToolCallId, b: &ToolCallId) -> bool {
    std::ptr::eq(a.as_str().as_ptr(), b.as_str().as_ptr())
}

impl RunCalls {
    /// Empties the bookkeeping for the next run, keeping the capacity unless
    /// a table is far larger than the run that just used it.
    fn clear(&mut self) {
        self.started.clear();
        super::reset_set(&mut self.started_set);
        super::reset_set(&mut self.ended);
        super::reset_set(&mut self.answered);
        self.bytes = 0;
    }

    fn start(&mut self, id: &ToolCallId) {
        if self.started_set.insert(id.clone()) {
            self.started.push(id.clone());
            self.bytes = self
                .bytes
                .saturating_add(STARTED_SLOT + super::id_heap(id.as_str().len()));
        }
        if let Some(ended) = self.ended.take(id) {
            self.bytes = self.bytes.saturating_sub(self.entry_bytes(&ended));
        }
    }

    /// What an `ended` or `answered` entry keyed by `key` is charged: its
    /// slot, and its id's heap unless it shares `started_set`'s `Arc`.
    fn entry_bytes(&self, key: &ToolCallId) -> usize {
        match self.started_set.get(key) {
            Some(started) if same_arc(started, key) => ID_SLOT,
            _ => ID_SLOT + super::id_heap(key.as_str().len()),
        }
    }

    /// Records `id` in `ended` (or `answered`), sharing `started_set`'s `Arc`
    /// when the run started the call.
    fn mark(&mut self, id: &ToolCallId, answered: bool) {
        let key = self.started_set.get(id).unwrap_or(id).clone();
        let charge = self.entry_bytes(&key);
        let set = if answered {
            &mut self.answered
        } else {
            &mut self.ended
        };
        if set.insert(key) {
            self.bytes = self.bytes.saturating_add(charge);
        }
    }
}

/// Message history and state for one thread.
#[derive(Debug, Clone)]
pub struct Reducer {
    messages: Vec<Message>,
    /// What each held message has been charged against `history_growth`, in
    /// step with `messages`: a drop or a replacement credits exactly this,
    /// so a charge can never be measured differently from its credit.
    charged: Vec<usize>,
    state: StateSlot,
    /// Message id → position in `messages`.
    index: HashMap<MessageId, usize>,
    /// Tool call id → where the call lives (the first holder, by position).
    calls: HashMap<ToolCallId, CallSlot>,
    /// The last content delta's target: `index`'s own key and the position
    /// it maps to, so a run of deltas to one message hashes its id once.
    /// Cleared by every event that is not a content delta (see
    /// [`keeps_hot_targets`]): only those leave `index` and `messages` as
    /// they were.
    hot_message: Option<(MessageId, usize)>,
    /// As `hot_message`, for the last `TOOL_CALL_ARGS` target in `calls`.
    hot_call: Option<(ToolCallId, CallSlot)>,
    /// Every holder `(message, position)` of a call id that two or more held
    /// calls share, so the mapping can fall back to the next holder when the
    /// first one is replaced or dropped. Empty in the normal case.
    call_dups: CallHolders,
    limits: PatchLimits,
    state_budget: PatchBudget,
    /// Node budgets of activity contents, created on first use.
    activity_budgets: ActivityBudgets,
    /// A patch was refused at the node-unit ceiling: a fatal verdict for the
    /// [`Processor`](super::Processor) to end the stream with.
    ceiling_hit: bool,
    /// Held messages by retention class (see [`class`]): `reconcile` needs
    /// the counts to find what a snapshot drops without scanning the history.
    class_counts: [usize; 3],
    /// Held copies of an id beyond the one `index` names, by class. Empty in
    /// the normal case; a reducer-minted carrier can share a held id.
    dup_extra: HashMap<MessageId, [u32; 3]>,
    /// Estimated bytes the history grew by since the limits were set
    /// (negative when it shrank), against `max_history_bytes`.
    history_growth: i64,
    max_history_bytes: usize,
    /// Work (bytes serialized) the next activity patch may spend measuring
    /// what it frees; failed patches spend it for good, so a peer cannot
    /// make failing frames cost more than O(frame) each, amortized.
    credit_bank: usize,
    /// Work (held messages walked and moved) the snapshots may spend dropping
    /// messages ahead of a long held tail; every message the peer sends
    /// refills it, so the work stays linear in the input. See [`RECONCILE_BANK`].
    reconcile_bank: usize,
    /// A snapshot was refused whole for want of `reconcile_bank`: a fatal
    /// verdict for the [`Processor`](super::Processor).
    reconcile_hit: bool,
    run_calls: RunCalls,
    pending: Vec<PendingToolCall>,
    /// Estimated bytes of the `pending` copies `finish_run` made, charged to
    /// the history budget until the next run starts (D25). Pending calls
    /// restored with the thread are not counted.
    pending_bytes: usize,
    /// The verifier's bookkeeping estimate last added to `history_growth`
    /// ([`set_bookkeeping`](Self::set_bookkeeping)).
    bookkeeping: usize,
    interrupts: Vec<Interrupt>,
    usage: Vec<TokenUsage>,
    /// Ids of the messages the latest run added, in order.
    run_ids: Vec<MessageId>,
    /// The latest content delta's `Arc`, reused for the next event when the
    /// application has dropped its handle (see the module docs).
    spare: Option<Arc<Event>>,
    /// Ids the reducer minted a parentless call's carrier under (the call's
    /// id) that the producer has not used as a message id, each with the
    /// run (`run_seq`) that added the carrier. The carrier gives such an id
    /// up when the producer opens a message under it (see the module docs).
    minted: HashMap<MessageId, u64>,
    /// Runs begun on this reducer, for `minted`.
    run_seq: u64,
    /// Fresh carrier ids handed out, for the next one's suffix.
    renames: u64,
}

/// A `minted` entry, charged to its carrier while it lasts (D25).
const MINTED_SLOT: usize = super::table_slot(16 + 8);

impl Default for Reducer {
    fn default() -> Self {
        Self::new()
    }
}

impl Reducer {
    /// An empty thread: no history, state `{}` (RUNIN-24).
    #[must_use]
    pub fn new() -> Self {
        Self::with_history(Vec::new(), Value::Object(serde_json::Map::new()))
    }

    /// A thread continuing from earlier history and state. Pending frontend
    /// calls are derived from the history: assistant `toolCalls` with no
    /// matching tool message (TOOL-32, TOOL-33).
    #[must_use]
    pub fn with_history(messages: Vec<Message>, state: Value) -> Self {
        let mut reducer = Self::restore(messages, state, Vec::new(), Vec::new());
        reducer.pending = reducer.unanswered_in_history();
        reducer
    }

    /// A thread restored exactly, pending calls and open interrupts included
    /// (from a [`ThreadSnapshot`][crate::client::ThreadSnapshot]).
    #[cfg_attr(
        not(feature = "client"),
        doc = "",
        doc = "[crate::client::ThreadSnapshot]: https://docs.rs/agui-sdk/latest/agui_sdk/client/struct.ThreadSnapshot.html"
    )]
    #[must_use]
    pub fn restore(
        messages: Vec<Message>,
        state: Value,
        pending: Vec<PendingToolCall>,
        interrupts: Vec<Interrupt>,
    ) -> Self {
        let limits = PatchLimits::default();
        let mut reducer = Reducer {
            run_ids: Vec::new(),
            messages,
            charged: Vec::new(),
            state_budget: PatchBudget::for_document(&state, limits),
            state: StateSlot::owned(state),
            index: HashMap::new(),
            calls: HashMap::new(),
            hot_message: None,
            hot_call: None,
            call_dups: HashMap::new(),
            limits,
            activity_budgets: ActivityBudgets::default(),
            ceiling_hit: false,
            class_counts: [0; 3],
            dup_extra: HashMap::new(),
            history_growth: 0,
            max_history_bytes: super::Limits::default().max_history_bytes,
            credit_bank: CREDIT_BANK,
            reconcile_bank: 0,
            reconcile_hit: false,
            minted: HashMap::new(),
            run_seq: 0,
            renames: 0,
            run_calls: RunCalls::default(),
            pending,
            pending_bytes: 0,
            bookkeeping: 0,
            interrupts,
            usage: Vec::new(),
            spare: None,
        };
        reducer.rebuild_indices();
        reducer.reconcile_bank = reducer.reconcile_allowance();
        reducer
    }

    /// Assistant tool calls in `messages` that no tool message answers, in
    /// history order.
    fn unanswered_in_history(&self) -> Vec<PendingToolCall> {
        let answered: HashSet<&ToolCallId> = self
            .messages
            .iter()
            .filter_map(|m| match m {
                Message::Tool(t) => Some(&t.tool_call_id),
                _ => None,
            })
            .collect();
        let mut pending = Vec::new();
        for m in &self.messages {
            if let Message::Assistant(a) = m {
                for call in a.tool_calls.iter().flatten() {
                    if !answered.contains(&call.id) {
                        pending.push(PendingToolCall::new(
                            call.id.clone(),
                            call.function.name.clone(),
                            call.function.arguments.clone(),
                            a.id.clone(),
                        ));
                    }
                }
            }
        }
        pending
    }

    /// Uses `limits` for every later state and activity patch, and adopts
    /// `max_history_bytes`. History growth then counts from the history held
    /// now, and the credit and reconcile work banks start over (the
    /// per-message charge ledger is kept). For
    /// [`Processor`](super::Processor), which owns the configured limits.
    pub(crate) fn set_limits(&mut self, limits: &super::Limits) {
        // Only at construction: no bookkeeping has been charged yet, so
        // zeroing the growth forgets none of it.
        debug_assert!(
            self.bookkeeping == 0 && self.run_calls.bytes == 0 && self.pending_bytes == 0,
            "set_limits after the stream began"
        );
        let limits_patch = limits.patch;
        self.max_history_bytes = limits.max_history_bytes;
        self.history_growth = 0;
        self.credit_bank = CREDIT_BANK;
        self.reconcile_bank = self.reconcile_allowance();
        self.set_patch_limits(limits_patch);
    }

    /// Adds what the pipeline's other stages keep under peer-chosen ids
    /// (`total`, estimated resident bytes) to the history growth, replacing
    /// the total reported last time (D25). For
    /// [`Processor`](super::Processor), after the verifier observed an event
    /// and before the reducer applies it.
    pub(crate) fn set_bookkeeping(&mut self, total: usize) {
        self.history_growth = self
            .history_growth
            .saturating_add(signed(total))
            .saturating_sub(signed(self.bookkeeping));
        self.bookkeeping = total;
    }

    /// Charges the change in the run's tool-call tables since `before`.
    fn settle_run_calls(&mut self, before: usize) {
        self.history_growth = self
            .history_growth
            .saturating_add(signed(self.run_calls.bytes))
            .saturating_sub(signed(before));
    }

    /// The reconcile work a thread holding the current history starts with:
    /// the one-off bank, and a refill for each held message.
    fn reconcile_allowance(&self) -> usize {
        RECONCILE_BANK.saturating_add(RECONCILE_REFILL.saturating_mul(self.messages.len()))
    }

    fn set_patch_limits(&mut self, limits: PatchLimits) {
        self.limits = limits;
        self.state_budget = PatchBudget::for_document(self.state.get(), limits);
        self.activity_budgets.clear();
    }

    /// Applies one verified event, taking ownership of it. Returns the event,
    /// now shared with the reducer, and what it changed. No payload is cloned
    /// here (see the module docs for when a copy-on-write copy can happen).
    ///
    /// The returned `Arc` reuses the previous content delta's allocation
    /// when the application has already dropped that update, so a stream of
    /// deltas allocates nothing per event (see the module docs).
    pub fn apply(&mut self, event: Event, reporter: &mut Reporter) -> super::Processed {
        let event = self.wrap(event);
        let changes = self.reduce(&event, reporter);
        if is_content_delta(&event) {
            self.spare = Some(Arc::clone(&event));
        }
        super::Processed { event, changes }
    }

    /// `Arc::new(event)`, in the spare allocation when nobody else holds it.
    /// The spare is released either way, before the event is reduced, so it
    /// never keeps another handle alive.
    fn wrap(&mut self, event: Event) -> Arc<Event> {
        if let Some(mut spare) = self.spare.take() {
            if let Some(slot) = Arc::get_mut(&mut spare) {
                *slot = event;
                return spare;
            }
        }
        Arc::new(event)
    }

    fn reduce(&mut self, shared: &Arc<Event>, reporter: &mut Reporter) -> Changes {
        let mut changes = Changes::default();
        if !keeps_hot_targets(shared) {
            self.hot_message = None;
            self.hot_call = None;
        }
        match &**shared {
            Event::RunStarted(_) => self.begin_run(),
            Event::RunFinished(e) => {
                self.usage = e.usage.clone().unwrap_or_default();
                self.interrupts = match &e.outcome {
                    Some(RunFinishedOutcome::Interrupt { interrupts }) => interrupts.clone(),
                    _ => Vec::new(),
                };
                self.finish_run();
            }
            Event::RunError(e) => {
                // Absent usage is "not reported" (LIFE-33): a late RUN_ERROR
                // after RUN_FINISHED keeps the usage that event reported.
                // Within a run nothing has set usage since RUN_STARTED
                // cleared it, so this changes nothing there.
                if let Some(usage) = &e.usage {
                    self.usage.clone_from(usage);
                }
                self.interrupts.clear();
                self.finish_run();
            }

            Event::TextMessageStart(e) => {
                let opening = TextOpening {
                    id: &e.message_id,
                    role: e.role.unwrap_or_default(),
                    name: e.name.as_deref(),
                    owner: e.subagent_run_id.as_ref(),
                };
                let i = self.open_text(opening, reporter);
                let n = merge_into(metadata_mut(&mut self.messages[i]), &e.metadata);
                self.charge_at(i, n);
                changes.message = Some(e.message_id.clone());
            }
            Event::TextMessageContent(e) => {
                changes.message = self.extend_message(
                    &e.message_id,
                    &e.delta,
                    &e.metadata,
                    reporter,
                    EventType::TextMessageContent,
                );
            }
            Event::TextMessageEnd(e) => {
                changes.message = self.extend_message(
                    &e.message_id,
                    "",
                    &e.metadata,
                    reporter,
                    EventType::TextMessageEnd,
                );
            }
            Event::TextMessageChunk(e) => {
                if let Some(id) = &e.message_id {
                    self.release_minted(id);
                    if !self.index.contains_key(id) {
                        let opening = TextOpening {
                            id,
                            role: e.role.unwrap_or_default(),
                            name: e.name.as_deref(),
                            owner: e.subagent_run_id.as_ref(),
                        };
                        self.open_text(opening, reporter);
                    }
                    let delta = e.delta.as_deref().unwrap_or("");
                    changes.message = self.extend_message(
                        id,
                        delta,
                        &e.metadata,
                        reporter,
                        EventType::TextMessageChunk,
                    );
                }
            }

            Event::ReasoningMessageStart(e) => {
                let i = self.open_reasoning(&e.message_id, e.subagent_run_id.as_ref(), reporter);
                let n = merge_into(metadata_mut(&mut self.messages[i]), &e.metadata);
                self.charge_at(i, n);
                changes.message = Some(e.message_id.clone());
            }
            Event::ReasoningMessageContent(e) => {
                changes.message = self.extend_message(
                    &e.message_id,
                    &e.delta,
                    &e.metadata,
                    reporter,
                    EventType::ReasoningMessageContent,
                );
            }
            Event::ReasoningMessageEnd(e) => {
                changes.message = self.extend_message(
                    &e.message_id,
                    "",
                    &e.metadata,
                    reporter,
                    EventType::ReasoningMessageEnd,
                );
            }
            Event::ReasoningMessageChunk(e) => {
                if let Some(id) = &e.message_id {
                    self.release_minted(id);
                    if !self.index.contains_key(id) {
                        self.open_reasoning(id, e.subagent_run_id.as_ref(), reporter);
                    }
                    let delta = e.delta.as_deref().unwrap_or("");
                    changes.message = self.extend_message(
                        id,
                        delta,
                        &e.metadata,
                        reporter,
                        EventType::ReasoningMessageChunk,
                    );
                }
            }
            Event::ReasoningEncryptedValue(e) => {
                changes.message =
                    self.store_encrypted(e.subtype, &e.entity_id, &e.encrypted_value, reporter);
            }

            Event::ToolCallStart(e) => {
                let opening = CallOpening {
                    id: &e.tool_call_id,
                    name: &e.tool_call_name,
                    parent: e.parent_message_id.as_ref(),
                    owner: e.subagent_run_id.as_ref(),
                };
                let slot = self.open_call(opening, reporter);
                changes.message = self.extend_call(slot, "", &e.metadata);
            }
            Event::ToolCallArgs(e) => {
                if let Some(slot) = self.args_target(&e.tool_call_id) {
                    changes.message = self.extend_call(slot, &e.delta, &e.metadata);
                } else {
                    report_missing_target(reporter, EventType::ToolCallArgs, "/toolCallId");
                }
            }
            Event::ToolCallEnd(e) => {
                if let Some(&slot) = self.calls.get(&e.tool_call_id) {
                    let before = self.run_calls.bytes;
                    self.run_calls.mark(&e.tool_call_id, false);
                    self.settle_run_calls(before);
                    // The arguments are complete: drop the growth slack, as
                    // `extend_message` does at TEXT_MESSAGE_END.
                    if let Some(call) = self.call_mut(slot) {
                        call.function.arguments.shrink_to_fit();
                    }
                    changes.message = self.extend_call(slot, "", &e.metadata);
                } else {
                    report_missing_target(reporter, EventType::ToolCallEnd, "/toolCallId");
                }
            }
            Event::ToolCallChunk(e) => {
                if let Some(id) = &e.tool_call_id {
                    let slot = match (self.calls.get(id), &e.tool_call_name) {
                        (Some(&slot), _) => Some(slot),
                        (None, Some(name)) => Some(self.open_call(
                            CallOpening {
                                id,
                                name,
                                parent: e.parent_message_id.as_ref(),
                                owner: e.subagent_run_id.as_ref(),
                            },
                            reporter,
                        )),
                        // A first chunk without a name cannot open a call (TOOL-09).
                        (None, None) => {
                            report_missing_target(
                                reporter,
                                EventType::ToolCallChunk,
                                "/toolCallId",
                            );
                            None
                        }
                    };
                    if let Some(slot) = slot {
                        let delta = e.delta.as_deref().unwrap_or("");
                        changes.message = self.extend_call(slot, delta, &e.metadata);
                    }
                }
            }
            Event::ToolCallResult(e) => {
                let message = ToolMessage {
                    id: e.message_id.clone(),
                    content: e.content.clone(),
                    tool_call_id: e.tool_call_id.clone(),
                    error: None,
                    encrypted_value: None,
                    metadata: e.metadata.clone(),
                    subagent_run_id: e.subagent_run_id.clone(),
                };
                // A carrier minted under this id gives it up first. Only a
                // tool message answering the same call is replaced (TS
                // parity); any other held message keeps its id and the
                // result is appended.
                self.release_minted(&e.message_id);
                let replaces = matches!(
                    self.index.get(&e.message_id).map(|&i| &self.messages[i]),
                    Some(Message::Tool(held)) if held.tool_call_id == e.tool_call_id
                );
                if replaces {
                    self.put_message(Message::Tool(message));
                } else {
                    self.push_message(Message::Tool(message));
                }
                let before = self.run_calls.bytes;
                self.run_calls.mark(&e.tool_call_id, true);
                self.settle_run_calls(before);
                changes.message = Some(e.message_id.clone());
            }

            Event::StateSnapshot(e) => {
                // Installed by sharing the event: no copy (STATE-01).
                self.state_budget = PatchBudget::for_document(&e.snapshot, self.limits);
                self.state = StateSlot::shared(Arc::clone(shared));
                changes.state = true;
            }
            Event::StateDelta(e) => {
                changes.state = self.patch_state(&e.delta, reporter);
            }
            Event::MessagesSnapshot(e) => {
                changes.messages_replaced = self.reconcile(&e.messages, reporter);
            }

            Event::ActivitySnapshot(e) => {
                changes.message = self.activity_snapshot(e, reporter);
            }
            Event::ActivityDelta(e) => {
                changes.message = self.activity_delta(e, reporter);
            }

            // Spans, steps, subagent lifecycle and passthrough events build no
            // item: nothing to reduce and no metadata target (META-14, PASS-03).
            _ => {}
        }
        changes
    }

    /// History so far, in order.
    #[must_use]
    pub fn messages(&self) -> &[Message] {
        &self.messages
    }

    /// One message by id, O(1).
    #[must_use]
    pub fn message(&self, id: &MessageId) -> Option<&Message> {
        self.index.get(id).map(|&i| &self.messages[i])
    }

    /// Current state.
    #[must_use]
    pub fn state(&self) -> &Value {
        self.state.get()
    }

    /// Frontend calls the latest run left unanswered (TOOL-32).
    #[must_use]
    pub fn pending_tool_calls(&self) -> &[PendingToolCall] {
        &self.pending
    }

    /// Interrupts the latest run ended with, if it ended interrupted.
    #[must_use]
    pub fn open_interrupts(&self) -> &[Interrupt] {
        &self.interrupts
    }

    /// Usage reported by the latest run's terminal events; a late
    /// `RUN_ERROR` without `usage` leaves `RUN_FINISHED`'s.
    #[must_use]
    pub fn usage(&self) -> &[TokenUsage] {
        &self.usage
    }

    /// Ids of the messages the latest run added, in order. Ids, not an index:
    /// a `MESSAGES_SNAPSHOT` may drop and reorder history (STATE-04..STATE-08).
    #[must_use]
    pub fn run_message_ids(&self) -> &[MessageId] {
        &self.run_ids
    }

    /// Takes history and state.
    #[must_use]
    pub fn into_parts(self) -> (Vec<Message>, Value) {
        (self.messages, self.state.into_value())
    }

    // ----- run lifecycle -----

    fn begin_run(&mut self) {
        self.run_ids.clear();
        self.run_seq += 1;
        let before = self.run_calls.bytes;
        self.run_calls.clear();
        self.settle_run_calls(before);
        self.history_growth = self
            .history_growth
            .saturating_sub(signed(std::mem::take(&mut self.pending_bytes)));
        self.pending.clear();
        self.interrupts.clear();
        self.usage.clear();
    }

    /// Derives the unanswered calls from the stream, never from
    /// `pendingToolCallIds` (TOOL-32).
    fn finish_run(&mut self) {
        let mut pending = Vec::new();
        for id in &self.run_calls.started {
            if self.run_calls.answered.contains(id) || !self.run_calls.ended.contains(id) {
                continue;
            }
            let Some(&slot) = self.calls.get(id) else {
                continue;
            };
            let Some(call) = self.call(slot) else {
                continue;
            };
            pending.push(PendingToolCall {
                tool_call_id: id.clone(),
                name: call.function.name.clone(),
                arguments: call.function.arguments.clone(),
                message_id: message_id(&self.messages[slot.message]).clone(),
            });
        }
        // The name and argument text are copies of the held calls' (D25).
        let bytes = pending.iter().fold(0_usize, |sum, call| {
            sum.saturating_add(std::mem::size_of::<PendingToolCall>())
                .saturating_add(text_bytes(call.name.len()))
                .saturating_add(text_bytes(call.arguments.len()))
        });
        let held = std::mem::replace(&mut self.pending_bytes, bytes);
        self.history_growth = self
            .history_growth
            .saturating_add(signed(bytes))
            .saturating_sub(signed(held));
        self.pending = pending;
    }

    // ----- indices -----

    fn rebuild_indices(&mut self) {
        self.hot_message = None;
        self.hot_call = None;
        self.index.clear();
        self.minted.clear();
        self.calls.clear();
        self.call_dups.clear();
        self.dup_extra.clear();
        self.class_counts = [0; 3];
        // Restored history is credited at its estimated size when dropped.
        self.charged.clear();
        self.charged.extend(self.messages.iter().map(message_bytes));
        // One table allocation, not a rehash at every doubling.
        self.index.reserve(self.messages.len());
        for (i, m) in self.messages.iter().enumerate() {
            self.class_counts[class(m)] += 1;
            // The first occurrence of an id names it, as everywhere else
            // (`call_parent`, `reconcile`).
            match self.index.entry(message_id(m).clone()) {
                std::collections::hash_map::Entry::Vacant(slot) => {
                    slot.insert(i);
                }
                std::collections::hash_map::Entry::Occupied(o) => {
                    self.dup_extra.entry(o.key().clone()).or_default()[class(m)] += 1;
                }
            }
            // Ascending order: the first holder names the call.
            map_calls(&mut self.calls, &mut self.call_dups, i, m);
        }
    }

    /// Appends `message`, or replaces the held message with its id in place.
    fn put_message(&mut self, message: Message) -> usize {
        match self.index.get(message_id(&message)) {
            Some(&i) => {
                self.swap_message(i, message);
                i
            }
            None => self.push_message(message),
        }
    }

    /// Replaces the message at `i` by one with the same id.
    fn swap_message(&mut self, i: usize, message: Message) {
        let id = message_id(&message).clone();
        if let Message::Assistant(old) = &self.messages[i] {
            for (position, call) in old.tool_calls.iter().flatten().enumerate() {
                unmap_call(&mut self.calls, &mut self.call_dups, &call.id, i, position);
            }
        }
        self.activity_budgets.remove(&id);
        // The replacement is the producer's: its id is no longer minted (its
        // charge goes with the old message's).
        self.minted.remove(&id);
        self.class_counts[class(&self.messages[i])] -= 1;
        self.class_counts[class(&message)] += 1;
        let bytes = message_bytes(&message);
        let held = std::mem::replace(&mut self.charged[i], bytes);
        self.history_growth = self
            .history_growth
            .saturating_add(signed(bytes))
            .saturating_sub(signed(held));
        self.insert_calls(i, &message);
        self.messages[i] = message;
    }

    /// Appends `message` as one of this run's. An id already held keeps
    /// naming the earlier message; the new one is reachable through its
    /// calls.
    fn push_message(&mut self, message: Message) -> usize {
        let i = self.messages.len();
        let id = message_id(&message).clone();
        self.class_counts[class(&message)] += 1;
        let bytes = message_bytes(&message);
        self.charged.push(bytes);
        self.history_growth = self.history_growth.saturating_add(signed(bytes));
        self.reconcile_bank = self.reconcile_bank.saturating_add(RECONCILE_REFILL);
        self.insert_calls(i, &message);
        match self.index.entry(id.clone()) {
            std::collections::hash_map::Entry::Vacant(slot) => {
                slot.insert(i);
            }
            std::collections::hash_map::Entry::Occupied(_) => {
                self.dup_extra.entry(id.clone()).or_default()[class(&message)] += 1;
            }
        }
        self.messages.push(message);
        self.run_ids.push(id);
        i
    }

    /// Maps the calls of the assistant `message` (at `at`) unless an earlier
    /// holder of the same call id exists (the first occurrence names it).
    fn insert_calls(&mut self, at: usize, message: &Message) {
        map_calls(&mut self.calls, &mut self.call_dups, at, message);
    }

    // ----- history accounting -----

    /// Charges `bytes` (negative when something was displaced) to the
    /// message at `i`. The message's ledger never goes below zero, and only
    /// the applied change reaches `history_growth`: a message can give back
    /// no more than it was charged.
    fn charge_at(&mut self, i: usize, bytes: i64) {
        let held = self.charged[i];
        let now = if bytes >= 0 {
            held.saturating_add(usize::try_from(bytes).unwrap_or(usize::MAX))
        } else {
            held.saturating_sub(usize::try_from(bytes.unsigned_abs()).unwrap_or(usize::MAX))
        };
        self.charged[i] = now;
        self.history_growth = self
            .history_growth
            .saturating_add(signed(now))
            .saturating_sub(signed(held));
    }

    // ----- text and reasoning messages -----

    /// Opens (creates or reopens) a text message; returns its position.
    fn open_text(&mut self, opening: TextOpening<'_>, reporter: &mut Reporter) -> usize {
        self.release_minted(opening.id);
        let Some(&i) = self.index.get(opening.id) else {
            return self.put_message(new_text_message(&opening));
        };
        let message = &mut self.messages[i];
        if let Message::Assistant(a) = message {
            if a.content.is_none() && opening.role == TextMessageRole::Assistant {
                // First text for a message minted by a tool call.
                a.content = Some(String::new());
                let mut added = 0;
                if a.name.is_none() {
                    a.name = opening.name.map(str::to_owned);
                    added = opening.name.map_or(0, str::len);
                }
                self.charge_at(i, signed(added));
                return i;
            }
        }
        let agrees = role_matches(message, opening.role)
            && message_name(message) == opening.name
            && message_owner(message) == opening.owner;
        if !agrees {
            reporter.report(|| {
                Diagnostic::new(
                    DiagnosticCode::ConflictingReopen,
                    Scope::Event,
                    "/messageId",
                    "reopening TEXT_MESSAGE_START disagrees with the message (role, name or owner); \
                     the established values are kept",
                )
                .with_event_type(EventType::TextMessageStart)
            });
        }
        i
    }

    fn open_reasoning(
        &mut self,
        id: &MessageId,
        owner: Option<&SubagentRunId>,
        reporter: &mut Reporter,
    ) -> usize {
        self.release_minted(id);
        let Some(&i) = self.index.get(id) else {
            return self.put_message(Message::Reasoning(ReasoningMessage {
                id: id.clone(),
                content: String::new(),
                encrypted_value: None,
                metadata: None,
                subagent_run_id: owner.cloned(),
            }));
        };
        let message = &self.messages[i];
        if !matches!(message, Message::Reasoning(_)) || message_owner(message) != owner {
            reporter.report(|| {
                Diagnostic::new(
                    DiagnosticCode::ConflictingReopen,
                    Scope::Event,
                    "/messageId",
                    "reopening REASONING_MESSAGE_START disagrees with the message (role or owner); \
                     the established values are kept",
                )
                .with_event_type(EventType::ReasoningMessageStart)
            });
        }
        i
    }

    /// Appends `delta` to a held message's text and merges `metadata` into it.
    /// A fragment for a message nothing holds changes nothing and is reported.
    fn extend_message(
        &mut self,
        id: &MessageId,
        delta: &str,
        metadata: &Option<Metadata>,
        reporter: &mut Reporter,
        event_type: EventType,
    ) -> Option<MessageId> {
        let Some(i) = self.message_target(id) else {
            report_missing_target(reporter, event_type, "/messageId");
            return None;
        };
        let message = &mut self.messages[i];
        if !delta.is_empty() {
            append_text(message, delta);
        } else if matches!(
            event_type,
            EventType::TextMessageEnd | EventType::ReasoningMessageEnd
        ) {
            // The text is complete: drop the growth slack, so the history
            // holds what it is charged for (D14). Charges are unchanged.
            shrink_text(message);
        }
        let n = merge_into(metadata_mut(message), metadata);
        self.charge_at(i, n.saturating_add(signed(delta.len())));
        Some(id.clone())
    }

    /// Where the held message `id` is: the last delta's target when it is
    /// the same message (an `Arc` pointer compare for a repeated id, no
    /// hashing), else `index`, remembered for the next delta.
    fn message_target(&mut self, id: &MessageId) -> Option<usize> {
        if let Some((hot, i)) = &self.hot_message {
            if hot == id {
                debug_assert_eq!(self.index.get(id), Some(i), "stale hot message");
                return Some(*i);
            }
        }
        let (key, &i) = self.index.get_key_value(id)?;
        // `index`'s own key: the cache keeps no id alive that the history
        // does not already hold (D25).
        self.hot_message = Some((key.clone(), i));
        Some(i)
    }

    /// [`message_target`](Self::message_target) for `TOOL_CALL_ARGS`.
    fn args_target(&mut self, id: &ToolCallId) -> Option<CallSlot> {
        if let Some((hot, slot)) = &self.hot_call {
            if hot == id {
                debug_assert_eq!(self.calls.get(id), Some(slot), "stale hot call");
                return Some(*slot);
            }
        }
        let (key, &slot) = self.calls.get_key_value(id)?;
        self.hot_call = Some((key.clone(), slot));
        Some(slot)
    }

    // ----- tool calls -----

    fn call(&self, slot: CallSlot) -> Option<&ToolCall> {
        match self.messages.get(slot.message)? {
            Message::Assistant(a) => a.tool_calls.as_ref()?.get(slot.position),
            _ => None,
        }
    }

    fn call_mut(&mut self, slot: CallSlot) -> Option<&mut ToolCall> {
        match self.messages.get_mut(slot.message)? {
            Message::Assistant(a) => a.tool_calls.as_mut()?.get_mut(slot.position),
            _ => None,
        }
    }

    /// Opens (creates or reopens) a tool call; returns where it lives.
    fn open_call(&mut self, opening: CallOpening<'_>, reporter: &mut Reporter) -> CallSlot {
        let before = self.run_calls.bytes;
        self.run_calls.start(opening.id);
        self.settle_run_calls(before);
        if let Some(&slot) = self.calls.get(opening.id) {
            return self.reopen_call(slot, opening, reporter);
        }
        let message = self.call_parent(opening.id, opening.parent, opening.owner);
        debug_assert!(matches!(self.messages[message], Message::Assistant(_)));
        let call = ToolCall {
            id: opening.id.clone(),
            kind: ToolCallType::Function,
            function: FunctionCall {
                name: opening.name.to_owned(),
                arguments: String::new(),
            },
            encrypted_value: None,
            metadata: None,
        };
        let position = match &mut self.messages[message] {
            Message::Assistant(a) => {
                let list = a.tool_calls.get_or_insert_with(Vec::new);
                list.push(call);
                list.len() - 1
            }
            // `call_parent` only returns assistant messages.
            _ => 0,
        };
        let slot = CallSlot { message, position };
        self.calls.insert(opening.id.clone(), slot);
        self.charge_at(
            message,
            signed(
                ITEM_OVERHEAD
                    .saturating_add(text_bytes(opening.id.as_str().len()))
                    .saturating_add(text_bytes(opening.name.len())),
            ),
        );
        slot
    }

    fn reopen_call(
        &mut self,
        slot: CallSlot,
        opening: CallOpening<'_>,
        reporter: &mut Reporter,
    ) -> CallSlot {
        let held = &self.messages[slot.message];
        // The parent differs when a new call would have gone somewhere else.
        let parent_differs = opening
            .parent
            .is_some_and(|p| self.resolve_parent(opening.id, p) != Some(slot.message));
        let owner_differs = message_owner(held) != opening.owner;
        let name_differs = self
            .call(slot)
            .is_some_and(|c| c.function.name != opening.name);
        if !(parent_differs || owner_differs || name_differs) {
            return slot;
        }
        reporter.report(|| {
            Diagnostic::new(
                DiagnosticCode::ConflictingReopen,
                Scope::Event,
                "/toolCallId",
                "reopening TOOL_CALL_START disagrees with the call (name, parent or owner); \
                 the latest name is applied, the established parent and owner are kept",
            )
            .with_event_type(EventType::ToolCallStart)
        });
        // The call stays where it is (the TypeScript reference never moves
        // one): moving would cost O(calls on the old message) per reopen.
        let mut delta = 0;
        if let Some(call) = self.call_mut(slot) {
            if call.function.name != opening.name {
                delta = signed(opening.name.len()) - signed(call.function.name.len());
                call.function.name = opening.name.to_owned();
            }
        }
        self.charge_at(slot.message, delta);
        slot
    }

    /// Where a new call under `parent` would join, if that is a message held
    /// now: the assistant message `parent` names, or the carrier minted under
    /// the call's own id when `parent` names another kind of message.
    fn resolve_parent(&self, call_id: &ToolCallId, parent: &MessageId) -> Option<usize> {
        match self.index.get(parent) {
            Some(&i) if matches!(self.messages[i], Message::Assistant(_)) => Some(i),
            Some(_) => self.assistant_at(&MessageId::new(call_id.as_arc().clone())),
            None => None,
        }
    }

    fn assistant_at(&self, id: &MessageId) -> Option<usize> {
        self.index
            .get(id)
            .copied()
            .filter(|&i| matches!(self.messages[i], Message::Assistant(_)))
    }

    /// The assistant message a new call joins (see the module docs), minted
    /// when needed. Always returns the position of an assistant message.
    fn call_parent(
        &mut self,
        call_id: &ToolCallId,
        parent: Option<&MessageId>,
        owner: Option<&SubagentRunId>,
    ) -> usize {
        match parent {
            Some(parent) => {
                if let Some(i) = self.assistant_at(parent) {
                    // Naming a carrier as a parent adopts its id: it is the
                    // producer's message from now on.
                    if self.minted.remove(parent).is_some() {
                        self.charge_at(i, -signed(MINTED_SLOT));
                    }
                    return i;
                }
                if !self.index.contains_key(parent) {
                    return self.put_message(new_call_carrier(parent.clone(), owner));
                }
            }
            None => {
                if let Some(m) = self.messages.last() {
                    let added_by_run = self.run_ids.last() == Some(message_id(m));
                    if added_by_run
                        && matches!(m, Message::Assistant(_))
                        && message_owner(m) == owner
                    {
                        return self.messages.len() - 1;
                    }
                }
            }
        }
        // Mint a carrier under the call's own id, unless that id is already
        // an assistant message (then join it).
        let carrier_id = MessageId::new(call_id.as_arc().clone());
        if let Some(i) = self.assistant_at(&carrier_id) {
            return i;
        }
        // An id clash with a non-assistant message keeps the index on the
        // earlier message; the carrier is reachable through its calls.
        let i = self.push_message(new_call_carrier(carrier_id.clone(), owner));
        if self.index.get(&carrier_id) == Some(&i) {
            self.minted.insert(carrier_id, self.run_seq);
            self.charge_at(i, signed(MINTED_SLOT));
        }
        i
    }

    /// A message the producer opens under `id`: when `id` names a carrier the
    /// reducer minted (not one the producer opened), the carrier is renamed
    /// to a fresh `<id>~<n>` (itself minted) so `id` is free for the
    /// producer's message. The carrier keeps its position and calls; the
    /// run's id list follows the rename.
    fn release_minted(&mut self, id: &MessageId) {
        let Some(run) = self.minted.remove(id) else {
            return;
        };
        let Some(&i) = self.index.get(id) else {
            return;
        };
        // Candidates never repeat (the suffix only grows), so each held id
        // blocks at most one, ever.
        let fresh = loop {
            self.renames += 1;
            let candidate = MessageId::new(format!("{}~{}", id.as_str(), self.renames));
            if !self.index.contains_key(&candidate) {
                break candidate;
            }
        };
        let Message::Assistant(carrier) = &mut self.messages[i] else {
            return;
        };
        carrier.id = fresh.clone();
        self.charge_at(
            i,
            signed(text_bytes(fresh.as_str().len())) - signed(text_bytes(id.as_str().len())),
        );
        self.index.remove(id);
        self.index.insert(fresh.clone(), i);
        // A later copy of `id`, if any, names it now.
        if self.dup_extra.contains_key(id) {
            if let Some(j) =
                (i + 1..self.messages.len()).find(|&j| message_id(&self.messages[j]) == id)
            {
                self.index.insert(id.clone(), j);
                let c = class(&self.messages[j]);
                self.dup_dec(id, c);
            }
        }
        if run == self.run_seq {
            // The run's messages are the history's tail (see `compact_from`).
            let at = (self.messages.len().checked_sub(self.run_ids.len()))
                .and_then(|base| i.checked_sub(base))
                .filter(|&k| self.run_ids.get(k) == Some(id));
            debug_assert!(at.is_some(), "a carrier of this run is in its tail");
            if let Some(k) = at {
                self.run_ids[k] = fresh.clone();
            }
        }
        self.minted.insert(fresh, run);
    }

    /// Appends argument text and merges metadata into the call itself, never
    /// into its message (META-10). Returns the carrying message's id.
    fn extend_call(
        &mut self,
        slot: CallSlot,
        delta: &str,
        metadata: &Option<Metadata>,
    ) -> Option<MessageId> {
        let call = self.call_mut(slot)?;
        call.function.arguments.push_str(delta);
        let n = merge_into(&mut call.metadata, metadata);
        self.charge_at(slot.message, n.saturating_add(signed(delta.len())));
        Some(message_id(&self.messages[slot.message]).clone())
    }

    // ----- encrypted values -----

    fn store_encrypted(
        &mut self,
        subtype: EncryptedValueSubtype,
        entity_id: &str,
        value: &str,
        reporter: &mut Reporter,
    ) -> Option<MessageId> {
        const NO_CALL: &str = "encrypted value names no known tool call; dropped";
        // (message, bytes of the value it held before)
        let stored: Result<(usize, usize), &'static str> = match subtype {
            EncryptedValueSubtype::Message => match self.index.get(entity_id) {
                Some(&i) => match encrypted_slot(&mut self.messages[i]) {
                    Some(slot) => {
                        let held = slot.as_ref().map_or(0, String::len);
                        *slot = Some(value.to_owned());
                        Ok((i, held))
                    }
                    None => Err(
                        "encrypted value names an activity message, which cannot hold one; dropped",
                    ),
                },
                None => Err("encrypted value names no known message; dropped"),
            },
            EncryptedValueSubtype::ToolCall => match self.calls.get(entity_id).copied() {
                Some(slot) => match self.call_mut(slot) {
                    Some(call) => {
                        let held = call.encrypted_value.as_ref().map_or(0, String::len);
                        call.encrypted_value = Some(value.to_owned());
                        Ok((slot.message, held))
                    }
                    None => Err(NO_CALL),
                },
                None => Err(NO_CALL),
            },
        };
        let message = match stored {
            Ok((i, held)) => {
                // Replacing a value is not growth.
                self.charge_at(i, signed(value.len()) - signed(held));
                Some(i)
            }
            Err(why) => {
                reporter.report(|| {
                    Diagnostic::new(
                        DiagnosticCode::UnknownEncryptedEntity,
                        Scope::Event,
                        "/entityId",
                        why,
                    )
                    .with_event_type(EventType::ReasoningEncryptedValue)
                });
                None
            }
        };
        message.map(|i| message_id(&self.messages[i]).clone())
    }

    /// Takes the fatal verdict the last [`apply`](Self::apply) reached, if
    /// any. Three verdicts, in this order:
    ///
    /// - the history grew past `max_history_bytes` (checked first, and not
    ///   cleared: later calls return it again);
    /// - a snapshot was refused whole for want of reconcile work (cleared
    ///   when taken);
    /// - a patch would have taken the state and activity contents past
    ///   [`PatchLimits::max_nodes`] (it was not applied and was reported;
    ///   cleared when taken).
    pub(crate) fn take_fatal(&mut self) -> Option<crate::error::Error> {
        let growth = usize::try_from(self.history_growth).unwrap_or(0);
        if growth > self.max_history_bytes {
            return Some(crate::error::Error::new(
                crate::error::ErrorCode::LimitExceeded,
                format!(
                    "the message history and the stream's id bookkeeping grew by an \
                     estimated {growth} bytes, past the {}-byte limit",
                    self.max_history_bytes
                ),
            ));
        }
        if std::mem::take(&mut self.reconcile_hit) {
            return Some(crate::error::Error::new(
                crate::error::ErrorCode::LimitExceeded,
                "a messages snapshot would move more held messages than the reconcile \
                 work bound allows",
            ));
        }
        if !std::mem::take(&mut self.ceiling_hit) {
            return None;
        }
        Some(crate::error::Error::new(
            crate::error::ErrorCode::LimitExceeded,
            format!(
                "a patch would take the state and activity content past the \
                 {}-node-unit ceiling",
                self.limits.max_nodes
            ),
        ))
    }

    /// Records a patch refused at the node-unit ceiling as fatal.
    fn ceiling_reached(&mut self, reason: PatchErrorReason) {
        if reason == PatchErrorReason::NodeCeilingExceeded {
            self.ceiling_hit = true;
        }
    }

    // ----- state -----

    fn patch_state(&mut self, delta: &[PatchOperation], reporter: &mut Reporter) -> bool {
        if delta.is_empty() {
            return false;
        }
        let ceiling = self
            .limits
            .max_nodes
            .saturating_sub(self.activity_budgets.nodes);
        let applied = patch::apply_checked(
            self.state.get_mut(),
            delta,
            &mut self.state_budget,
            ceiling,
            |_| true,
        );
        match applied {
            Ok(()) => true,
            Err(CheckedError::Refused) => false,
            Err(CheckedError::Patch(e)) => {
                self.ceiling_reached(e.reason);
                report_patch_failure(
                    reporter,
                    EventType::StateDelta,
                    "/delta",
                    delta,
                    e.index,
                    e.reason,
                    "state",
                );
                false
            }
        }
    }

    // ----- messages snapshot -----

    /// STATE-04..STATE-08: replace held messages in place, append unseen ones
    /// in snapshot order, drop held ones the snapshot omits except
    /// client-only roles the snapshot does not restate.
    ///
    /// The cost is O(snapshot) plus the held tail from the first dropped
    /// message on: what the snapshot changes is found from the id index and
    /// the class counts, never by walking the whole history. A snapshot
    /// refused at the work bound changes nothing. Returns whether the
    /// snapshot was applied.
    fn reconcile(&mut self, snapshot: &[Message], reporter: &mut Reporter) -> bool {
        self.reconcile_bank = self
            .reconcile_bank
            .saturating_add(RECONCILE_REFILL.saturating_mul(snapshot.len()));
        let restates_activity = snapshot.iter().any(|m| matches!(m, Message::Activity(_)));
        let restates_reasoning = snapshot.iter().any(|m| matches!(m, Message::Reasoning(_)));
        // Only the first occurrence of an id in the snapshot counts.
        let mut first: HashMap<&MessageId, usize> = HashMap::with_capacity(snapshot.len());
        let mut replaces: Vec<(usize, usize)> = Vec::new();
        let mut appends: Vec<usize> = Vec::new();
        // Held copies of restated ids: the one `index` names, and the extras.
        let mut named = [0usize; 3];
        let mut extra = [0usize; 3];
        for (j, m) in snapshot.iter().enumerate() {
            let id = message_id(m);
            match first.entry(id) {
                std::collections::hash_map::Entry::Occupied(_) => continue,
                std::collections::hash_map::Entry::Vacant(slot) => {
                    slot.insert(j);
                }
            }
            match self.index.get(id) {
                Some(&pos) => {
                    replaces.push((pos, j));
                    named[class(&self.messages[pos])] += 1;
                    for (c, n) in self.dup_extra.get(id).into_iter().flatten().enumerate() {
                        extra[c] += *n as usize;
                    }
                }
                None => appends.push(j),
            }
        }
        // What the snapshot leaves held: its restated messages, and the
        // client-only roles it does not restate.
        let mut kept = replaces.len();
        if !restates_activity {
            kept += self.class_counts[1].saturating_sub(named[1] + extra[1]);
        }
        if !restates_reasoning {
            kept += self.class_counts[2].saturating_sub(named[2] + extra[2]);
        }
        let drops = self.messages.len().saturating_sub(kept);

        let keeps = |this: &Self, pos: usize, m: &Message| -> bool {
            let id = message_id(m);
            if first.contains_key(id) {
                this.index.get(id) == Some(&pos)
            } else {
                match m {
                    Message::Activity(_) => !restates_activity,
                    Message::Reasoning(_) => !restates_reasoning,
                    _ => false,
                }
            }
        };
        let mut compaction: Option<(usize, usize)> = None;
        if drops > 0 {
            // The first dropped position, found from the end. Walking to it,
            // and closing the gaps behind it, costs the held tail: that is
            // paid from the bank, so tiny snapshots cannot make every frame
            // O(held). The walk only reads positions and ids, which the
            // replaces below do not change, so it runs before any mutation.
            let mut remaining = drops;
            let mut first_dropped = self.messages.len();
            let mut walked = 0usize;
            while remaining > 0 && first_dropped > 0 && walked < self.reconcile_bank {
                first_dropped -= 1;
                walked += 1;
                if !keeps(self, first_dropped, &self.messages[first_dropped]) {
                    remaining -= 1;
                }
            }
            if remaining > 0 {
                debug_assert!(first_dropped > 0, "drop count and walk disagree");
                // Out of work: the walk's steps are spent, the snapshot is
                // refused whole.
                self.reconcile_bank = 0;
                self.reconcile_hit = true;
                reporter.report(|| {
                    Diagnostic::new(
                        DiagnosticCode::SnapshotWorkBound,
                        Scope::Event,
                        "/messages",
                        "the snapshot would drop held messages behind a long held tail, past \
                         the reconcile work bound; the snapshot is not applied",
                    )
                    .with_event_type(EventType::MessagesSnapshot)
                });
                return false;
            }
            compaction = Some((first_dropped, walked));
        }

        for (pos, j) in replaces {
            self.swap_message(pos, snapshot[j].clone());
        }

        if let Some((first_dropped, walked)) = compaction {
            self.reconcile_bank -= walked;
            self.compact_from(first_dropped, keeps);
        }

        for j in appends {
            self.push_message(snapshot[j].clone());
        }
        true
    }

    /// Drops the messages `keeps` refuses from `from` on, closing the gaps
    /// and renumbering only what moved. `run_ids` loses the dropped ones, by
    /// position, so it stays exactly the history's tail.
    fn compact_from(&mut self, from: usize, keeps: impl Fn(&Self, usize, &Message) -> bool) {
        // The run's messages are the history's tail (they are only ever
        // appended, and compaction keeps order): `run_ids[k]` names the
        // message at `run_base + k`, duplicate ids included.
        let run_base = self.messages.len().saturating_sub(self.run_ids.len());
        let run_from = from.max(run_base);
        let run_tail = self.run_ids.split_off(run_from - run_base);
        let mut run_tail = run_tail.into_iter();
        let tail = self.messages.split_off(from);
        let charges = self.charged.split_off(from);
        for (offset, (m, charge)) in tail.into_iter().zip(charges).enumerate() {
            let old = from + offset;
            let id = message_id(&m).clone();
            let run_id = if old >= run_from {
                run_tail.next()
            } else {
                None
            };
            if keeps(self, old, &m) {
                self.run_ids.extend(run_id);
                let new = self.messages.len();
                match self.index.get(&id) {
                    // A copy outlived the one that named the id: it names it now.
                    None => {
                        self.index.insert(id.clone(), new);
                        self.dup_dec(&id, class(&m));
                    }
                    Some(&at) if at == old && new != old => {
                        self.index.insert(id.clone(), new);
                    }
                    _ => {}
                }
                if new != old {
                    if let Message::Assistant(a) = &m {
                        for (position, call) in a.tool_calls.iter().flatten().enumerate() {
                            remap_call(
                                &mut self.calls,
                                &mut self.call_dups,
                                &call.id,
                                (old, new),
                                position,
                            );
                        }
                    }
                }
                self.messages.push(m);
                self.charged.push(charge);
            } else {
                self.class_counts[class(&m)] -= 1;
                self.history_growth = self.history_growth.saturating_sub(signed(charge));
                if self.index.get(&id) == Some(&old) {
                    self.index.remove(&id);
                    self.minted.remove(&id);
                    self.activity_budgets.remove(&id);
                } else {
                    self.dup_dec(&id, class(&m));
                }
                if let Message::Assistant(a) = &m {
                    for (position, call) in a.tool_calls.iter().flatten().enumerate() {
                        unmap_call(
                            &mut self.calls,
                            &mut self.call_dups,
                            &call.id,
                            old,
                            position,
                        );
                    }
                }
            }
        }
        debug_assert!(run_tail.next().is_none(), "run ids outnumber the tail");
    }

    fn dup_dec(&mut self, id: &MessageId, class: usize) {
        if let Some(d) = self.dup_extra.get_mut(id) {
            d[class] = d[class].saturating_sub(1);
            if *d == [0; 3] {
                self.dup_extra.remove(id);
            }
        }
    }

    // ----- activity -----

    fn activity_snapshot(
        &mut self,
        e: &ActivitySnapshot,
        reporter: &mut Reporter,
    ) -> Option<MessageId> {
        self.release_minted(&e.message_id);
        let Some(&i) = self.index.get(&e.message_id) else {
            // ACT-02: a new activity message at its arrival position.
            self.put_message(Message::Activity(ActivityMessage {
                id: e.message_id.clone(),
                activity_type: e.activity_type.clone(),
                content: e.content.clone(),
                metadata: e.metadata.clone(),
                subagent_run_id: e.subagent_run_id.clone(),
            }));
            return Some(e.message_id.clone());
        };
        let Message::Activity(activity) = &mut self.messages[i] else {
            report_missing_activity(reporter, EventType::ActivitySnapshot);
            return None;
        };
        let mut growth: i64 = 0;
        if e.replace != Some(false) {
            // ACT-03, ACT-06: replace content and type, re-mint ownership.
            let old = resident_bytes(&activity.content);
            let new = resident_bytes(&e.content);
            let owner =
                |o: &Option<SubagentRunId>| o.as_ref().map_or(0, |r| text_bytes(r.as_str().len()));
            growth = signed(new) - signed(old) + signed(text_bytes(e.activity_type.len()))
                - signed(text_bytes(activity.activity_type.len()))
                + signed(owner(&e.subagent_run_id))
                - signed(owner(&activity.subagent_run_id));
            activity.content = e.content.clone();
            activity.activity_type = e.activity_type.clone();
            activity.subagent_run_id = e.subagent_run_id.clone();
            self.activity_budgets.remove(&e.message_id);
        }
        // ACT-05, META-12: metadata keeps merging either way.
        let n = merge_into(&mut activity.metadata, &e.metadata);
        self.charge_at(i, growth.saturating_add(n));
        Some(e.message_id.clone())
    }

    fn activity_delta(&mut self, e: &ActivityDelta, reporter: &mut Reporter) -> Option<MessageId> {
        let i = match self.index.get(&e.message_id) {
            Some(&i) if matches!(self.messages[i], Message::Activity(_)) => i,
            // ACT-12..ACT-14: skipped with a warning, never invented, never fatal.
            _ => {
                report_missing_activity(reporter, EventType::ActivityDelta);
                return None;
            }
        };
        let Message::Activity(activity) = &mut self.messages[i] else {
            return None;
        };
        let limits = self.limits;
        let headroom = self
            .max_history_bytes
            .saturating_sub(usize::try_from(self.history_growth).unwrap_or(0));
        let mut content = std::mem::take(&mut activity.content).into_value();
        // Nothing to credit without a limit to measure against.
        let (credit, spent, credited) = if self.max_history_bytes == usize::MAX {
            (0, 0, std::collections::HashSet::new())
        } else {
            let size = wire_bytes(&e.patch);
            self.credit_bank = self
                .credit_bank
                .saturating_add(size.saturating_mul(8))
                .min(CREDIT_BANK);
            patch_credit(&content, &e.patch, self.credit_bank)
        };
        let budgets = &mut self.activity_budgets;
        let budget = match budgets.map.entry(e.message_id.clone()) {
            std::collections::hash_map::Entry::Occupied(o) => o.into_mut(),
            std::collections::hash_map::Entry::Vacant(v) => {
                let b = PatchBudget::for_document(&content, limits);
                budgets.nodes += b.nodes();
                v.insert(b)
            }
        };
        // What the state and the other activity contents leave of the
        // stream-wide ceiling.
        let before = budget.nodes();
        let others = (budgets.nodes - before).saturating_add(self.state_budget.nodes());
        let ceiling = limits.max_nodes.saturating_sub(others);
        // Copies are measured as they happen, only when there is a limit to
        // count them against.
        let measure = self.max_history_bytes != usize::MAX;
        let outcome = patch::apply_measured(
            &mut content,
            &e.patch,
            budget,
            ceiling,
            measure,
            Value::is_object,
        );
        budgets.nodes = budgets.nodes - before + budget.nodes();
        let (patched, credit) = match &outcome {
            Ok(measured) => {
                // The slots the patch vacated, outside what it credited
                // whole.
                let bank = self.credit_bank.saturating_sub(spent);
                let credit = credit.saturating_add(slot_credit(
                    &e.patch,
                    &measured.vacated,
                    &credited,
                    bank,
                ));
                // The estimate may stop once it passes what the limit and
                // the credits leave: the charge is over the limit by then.
                let bound = headroom.saturating_add(credit);
                (patch_bytes(&e.patch, measured.copied, bound), credit)
            }
            Err(_) => (0, credit),
        };
        // The content is an object before and (checked) after the patch.
        activity.content = crate::json::JsonObject::from_value(content).unwrap_or_default();
        if outcome.is_err() {
            // Work on a patch that did not apply is not paid for by any
            // freed bytes: it comes out of the bank.
            self.credit_bank = self.credit_bank.saturating_sub(spent);
        }
        match outcome {
            Ok(_) => {
                // ACT-09: a delta may retype the activity.
                let mut retyped = 0;
                if activity.activity_type != e.activity_type {
                    retyped = signed(e.activity_type.len()) - signed(activity.activity_type.len());
                    activity.activity_type = e.activity_type.clone();
                }
                let n = merge_into(&mut activity.metadata, &e.metadata);
                self.charge_at(
                    i,
                    n.saturating_add(signed(patched))
                        .saturating_add(retyped)
                        .saturating_sub(signed(credit)),
                );
                Some(e.message_id.clone())
            }
            Err(CheckedError::Patch(err)) => {
                self.ceiling_reached(err.reason);
                report_patch_failure(
                    reporter,
                    EventType::ActivityDelta,
                    "/patch",
                    &e.patch,
                    err.index,
                    err.reason,
                    "activity content",
                );
                None
            }
            Err(CheckedError::Refused) => {
                reporter.report(|| {
                    Diagnostic::new(
                        DiagnosticCode::PatchNotApplied,
                        Scope::Event,
                        "/patch",
                        "activity patch would leave content that is not an object; \
                         the prior content is kept",
                    )
                    .with_event_type(EventType::ActivityDelta)
                });
                None
            }
        }
    }
}

/// Node budgets of activity contents, created on first patch, and their
/// total: with the state's, it counts against [`PatchLimits::max_nodes`], so
/// many patched documents cannot multiply the ceiling.
#[derive(Debug, Clone, Default)]
struct ActivityBudgets {
    map: HashMap<MessageId, PatchBudget>,
    nodes: usize,
}

impl ActivityBudgets {
    fn remove(&mut self, id: &MessageId) {
        if let Some(b) = self.map.remove(id) {
            self.nodes -= b.nodes();
        }
    }

    fn clear(&mut self) {
        self.map.clear();
        self.nodes = 0;
    }
}

/// Descriptive fields of a text message opening (start or first chunk).
struct TextOpening<'a> {
    id: &'a MessageId,
    role: TextMessageRole,
    name: Option<&'a str>,
    owner: Option<&'a SubagentRunId>,
}

/// Descriptive fields of a tool call opening (start or first chunk).
struct CallOpening<'a> {
    id: &'a ToolCallId,
    name: &'a str,
    parent: Option<&'a MessageId>,
    owner: Option<&'a SubagentRunId>,
}

fn report_missing_target(reporter: &mut Reporter, event_type: EventType, member: &'static str) {
    reporter.report(|| {
        Diagnostic::new(
            DiagnosticCode::MissingFragmentTarget,
            Scope::Event,
            member,
            "names no held message or call (a snapshot may have dropped it); \
             the fragment is discarded",
        )
        .with_event_type(event_type)
    });
}

fn report_missing_activity(reporter: &mut Reporter, event_type: EventType) {
    reporter.report(|| {
        Diagnostic::new(
            DiagnosticCode::MissingActivityTarget,
            Scope::Event,
            "/messageId",
            "names no activity message; skipped",
        )
        .with_event_type(event_type)
    });
}

/// SNAP-12: a warning naming the failing operation, its kind and why.
fn report_patch_failure(
    reporter: &mut Reporter,
    event_type: EventType,
    member: &'static str,
    operations: &[PatchOperation],
    index: usize,
    reason: PatchErrorReason,
    target: &'static str,
) {
    reporter.report(|| {
        let code = match reason {
            PatchErrorReason::BudgetExceeded
            | PatchErrorReason::NodeCeilingExceeded
            | PatchErrorReason::TooDeep => DiagnosticCode::PatchBudgetExceeded,
            _ => DiagnosticCode::PatchNotApplied,
        };
        let op = operations.get(index).map_or("?", |o| op_name(o.op));
        Diagnostic::new(
            code,
            Scope::Event,
            format!("{member}/{index}"),
            // Leads with the wording the conformance corpus pins
            // (`state-delta-unappliable-warns-and-keeps`).
            format!(
                "Failed to apply {target} patch: operation {index} ({op}): {}; \
                 the prior {target} is kept",
                reason.as_str()
            ),
        )
        .with_event_type(event_type)
    });
}

fn op_name(op: patch::PatchOp) -> &'static str {
    match op {
        patch::PatchOp::Add => "add",
        patch::PatchOp::Remove => "remove",
        patch::PatchOp::Replace => "replace",
        patch::PatchOp::Move => "move",
        patch::PatchOp::Copy => "copy",
        patch::PatchOp::Test => "test",
    }
}

/// META-06, META-07: shallow, key-by-key, last write wins.
/// Returns the estimated net resident bytes the merge added to the history:
/// what `newer` holds, less what its keys displaced. O(`newer` + displaced).
fn merge_into(target: &mut Option<Metadata>, newer: &Option<Metadata>) -> i64 {
    let Some(newer) = newer else {
        return 0;
    };
    match target {
        Some(held) => {
            // A key already held keeps its entry: the value changes. A new
            // key adds its heap and an entry to the held map. `newer`'s own
            // map is dropped.
            let mut net: i64 = 0;
            let mut new_keys = 0;
            for (key, value) in newer.iter() {
                let added = signed(resident_bytes(value));
                net = net.saturating_add(match held.get(key) {
                    Some(old) => added.saturating_sub(signed(resident_bytes(old))),
                    None => {
                        new_keys += 1;
                        added.saturating_add(signed(text_bytes(key.len())))
                    }
                });
            }
            let grown = map_bytes(held.len() + new_keys).saturating_sub(map_bytes(held.len()));
            held.merge_from(newer.clone());
            net.saturating_add(signed(grown))
        }
        None => {
            *target = Some(newer.clone());
            signed(resident_bytes(newer))
        }
    }
}

/// A lower estimate of the bytes a patch frees from `content` (measured
/// before it is applied, credited only if it applied): the values `add`,
/// `replace` and `move` overwrite and `remove` drops (the slots they leave
/// are [`slot_credit`]'s), and the values the patch itself wrote at members
/// of objects the content already held and then freed again. It can only
/// under-count: each original location is credited once (nested ones not
/// at all), a value the patch wrote is credited at most what
/// [`patch_bytes`] charged for it, a `move` source and its ancestors are
/// never credited, crediting stops at the first operation that could shift
/// array indexes. A value the patch writes inside an array, inside an
/// object it created, or by `copy`, and then frees, is not credited. Measuring walks at most
/// `budget` bytes' worth of pre-patch nodes (strings are sized, not scanned;
/// a value the patch writes at a held object's member is walked outside the
/// budget, at most its own wire bytes, so that walk is linear in the patch
/// and cannot starve the pre-patch credit), and the
/// path prefixes the ancestor checks hash come out of the same budget
/// (exhausting it credits nothing); returns the credit and the work
/// spent, which the caller takes from its bank if the patch fails, and the
/// paths whose values it credited (for [`slot_credit`]). Failing patches
/// therefore cost O(patch) amortized, after a one-time [`CREDIT_BANK`].
///
/// A `remove` that shifts an array is credited, and ends the crediting:
/// nothing before it moved an index, so its path still names the value the
/// content held before the patch.
fn patch_credit<'a>(
    content: &Value,
    operations: &'a [PatchOperation],
    budget: usize,
) -> (usize, usize, std::collections::HashSet<&'a str>) {
    let mut remaining = budget;
    let mut entries: Vec<(&str, usize)> = Vec::new();
    let mut seen: std::collections::HashSet<&str> = std::collections::HashSet::new();
    let shifts = |path: &str| {
        path.rfind('/')
            .is_some_and(|at| matches!(content.pointer(&path[..at]), Some(Value::Array(_))))
    };
    // Values this patch wrote at members of objects the content held before
    // it, with what they were charged: when the same patch frees one again
    // (a later `remove`, `replace`, overwriting `add` or `move` onto it), it
    // is credited from here, as the pre-patch content does not hold it. A
    // member of such an object is addressed by key, so no index shift can
    // make the path name another value; a record under an ancestor the patch
    // touched is dropped by the ancestor filter below, like every entry.
    let mut written: std::collections::HashMap<&str, usize> = std::collections::HashMap::new();
    'ops: for op in operations {
        let path = op.path.as_str();
        let targets: [Option<(&str, bool, bool)>; 2] = match op.op {
            patch::PatchOp::Test => continue,
            // (path, credited, may shift an array)
            patch::PatchOp::Replace => [Some((path, true, false)), None],
            patch::PatchOp::Add | patch::PatchOp::Copy => [Some((path, true, true)), None],
            patch::PatchOp::Remove => [Some((path, true, true)), None],
            patch::PatchOp::Move => [
                op.from.as_ref().map(|f| (f.as_str(), false, true)),
                Some((path, true, true)),
            ],
        };
        // The record a `move` carries from its source to its target.
        let mut moving = None;
        for (p, credited, may_shift) in targets.into_iter().flatten() {
            let last = may_shift && shifts(p);
            if last && !matches!(op.op, patch::PatchOp::Remove) {
                break 'ops;
            }
            if seen.insert(p) {
                let mut amount = 0;
                if credited {
                    if let Some(old) = content.pointer(p) {
                        match freed_within(old, remaining) {
                            Some((work, freed)) => {
                                remaining -= work;
                                amount = freed;
                            }
                            None => {
                                remaining = 0;
                                break 'ops;
                            }
                        }
                    }
                }
                entries.push((p, amount));
            } else if credited {
                // Touched before: what the pre-patch content held here is
                // already counted; what this patch wrote here is freed now.
                if let Some(bytes) = written.remove(p) {
                    entries.push((p, bytes));
                }
            }
            if last {
                break 'ops;
            }
            if !credited {
                moving = written.remove(p);
                continue;
            }
            // What `op` leaves at `p`, if it is a value this patch charged
            // at a member of an object the content held. The parent is
            // checked first, so only a value that would be recorded is
            // walked. That walk is not taken from `remaining`: it visits at
            // most the value's own wire bytes, once per operation, so it is
            // linear in the patch the peer sent (which the engine copies in
            // full anyway), and it must not starve the pre-patch credit.
            let may_place = match op.op {
                patch::PatchOp::Add | patch::PatchOp::Replace => op.value.is_some(),
                patch::PatchOp::Move => moving.is_some(),
                _ => false,
            };
            let parent_held = may_place
                && p.rfind('/').is_some_and(|at| {
                    remaining = remaining.saturating_sub(at);
                    matches!(content.pointer(&p[..at]), Some(Value::Object(_)))
                });
            let placed = if parent_held {
                match op.op {
                    patch::PatchOp::Add | patch::PatchOp::Replace => op
                        .value
                        .as_ref()
                        .and_then(|value| freed_within(value, usize::MAX))
                        .map(|(_, bytes)| bytes),
                    patch::PatchOp::Move => moving.take(),
                    _ => None,
                }
            } else {
                None
            };
            match placed {
                Some(bytes) => {
                    written.insert(p, bytes);
                }
                None => {
                    written.remove(p);
                }
            }
        }
    }
    // A value moved out from under a credited ancestor survives, so an
    // ancestor of any `move` source (a `move` onto an ancestor of its own
    // source included) is not credited. Every prefix hashed is paid for from
    // the same budget; running out credits nothing.
    let mut pay = |n: usize| match remaining.checked_sub(n) {
        Some(r) => {
            remaining = r;
            true
        }
        None => {
            remaining = 0;
            false
        }
    };
    let mut ancestors: std::collections::HashSet<&str> = std::collections::HashSet::new();
    for f in operations
        .iter()
        .filter(|o| matches!(o.op, patch::PatchOp::Move))
        .filter_map(|o| o.from.as_ref().map(|f| f.as_str()))
    {
        for (at, _) in f.match_indices('/') {
            if !pay(at.saturating_add(1)) {
                return (0, budget, std::collections::HashSet::new());
            }
            ancestors.insert(&f[..at]);
        }
    }
    let mut credit = 0usize;
    let mut credited = std::collections::HashSet::new();
    'entries: for (p, n) in &entries {
        if ancestors.contains(p) {
            continue;
        }
        let mut rest = *p;
        while let Some(at) = rest.rfind('/') {
            rest = &rest[..at];
            if !pay(at.saturating_add(1)) {
                return (0, budget, std::collections::HashSet::new());
            }
            if seen.contains(rest) {
                continue 'entries;
            }
        }
        credit = credit.saturating_add(*n);
        credited.insert(*p);
    }
    (credit, budget - remaining, credited)
}

/// The credit for the slots a patch that applied vacated (`vacated`, from
/// the engine: each `remove` and `move` source's share of its container's
/// storage, and the member's key, at the length it was vacated at, so the
/// credits for a container's slots never add up to more than its slots
/// were charged). A slot whose container, or an ancestor of it, is in
/// `credited` is skipped: [`patch_credit`] credited that container whole,
/// slot included. Each prefix hashed costs its length from `budget`;
/// running out credits nothing.
fn slot_credit(
    operations: &[PatchOperation],
    vacated: &[(usize, usize)],
    credited: &std::collections::HashSet<&str>,
    budget: usize,
) -> usize {
    let mut remaining = budget;
    let mut credit = 0usize;
    'slots: for &(index, n) in vacated {
        let Some(op) = operations.get(index) else {
            continue;
        };
        let path = match op.op {
            patch::PatchOp::Move => op.from.as_ref().map_or("", |f| f.as_str()),
            _ => op.path.as_str(),
        };
        if !credited.is_empty() {
            // The container and each ancestor: every proper prefix.
            let mut rest = path;
            while let Some(at) = rest.rfind('/') {
                rest = &rest[..at];
                match remaining.checked_sub(at.saturating_add(1)) {
                    Some(r) => remaining = r,
                    None => return 0,
                }
                if credited.contains(rest) {
                    continue 'slots;
                }
            }
        }
        credit = credit.saturating_add(n);
    }
    credit
}

/// Serialization work, in bytes, banked for measuring what activity patches
/// free.
const CREDIT_BANK: usize = 64 * 1024;

/// Held messages a `MESSAGES_SNAPSHOT` may walk and move beyond what the
/// input paid for, for one-off compactions.
const RECONCILE_BANK: usize = 1 << 20;

/// Reconcile work each message a snapshot restates or the stream pushes (and
/// each message restored) pays for. The bank is not capped: it only banks
/// work the input has already paid for, so the total stays linear in it.
const RECONCILE_REFILL: usize = 8;

/// The work and the credit of measuring what dropping `value` frees, or
/// `None` once the work passes `cap`.
///
/// The work is a lower bound of [`wire_bytes`] (a string counts `len + 2`,
/// as escaping only adds bytes): the walk visits at most `cap` nodes (every
/// node is at least one byte) and never scans a string. The credit is a
/// lower bound of what the value was charged, whether it arrived whole
/// ([`resident_bytes`]) or was built by patches (each value placed charged
/// its [`crate::json::slot_bytes`], each vacated slot credited the same by
/// [`slot_credit`], so what is held of a container was charged at least its
/// [`map_bytes`] or [`seq_bytes`] at its current length): [`resident_bytes`]
/// of the value, which leaves out the slot the value itself sits in (that
/// is [`slot_credit`]'s).
fn freed_within(value: &Value, cap: usize) -> Option<(usize, usize)> {
    fn go(v: &Value, total: &mut usize, freed: &mut usize, cap: usize) -> bool {
        let held = match v {
            Value::Null | Value::Bool(_) | Value::Number(_) => 0,
            Value::String(s) => text_bytes(s.len()),
            Value::Array(a) => seq_bytes(a.len()),
            Value::Object(o) => map_bytes(o.len()),
        };
        *freed = freed.saturating_add(held);
        let n = match v {
            Value::Null | Value::Bool(_) => 4,
            Value::Number(_) => 1,
            Value::String(s) => s.len().saturating_add(2),
            Value::Array(a) => a.len().saturating_add(1),
            // Braces, colons and commas: `2 * len + 1`, or 2 when empty.
            Value::Object(o) => o.len().saturating_mul(2).saturating_add(1).max(2),
        };
        *total = total.saturating_add(n);
        if *total > cap {
            return false;
        }
        match v {
            Value::Array(a) => a.iter().all(|x| go(x, total, freed, cap)),
            Value::Object(o) => o.iter().all(|(k, x)| {
                *total = total.saturating_add(k.len().saturating_add(2));
                *freed = freed.saturating_add(text_bytes(k.len()));
                *total <= cap && go(x, total, freed, cap)
            }),
            _ => true,
        }
    }
    let (mut total, mut freed) = (0, 0);
    go(value, &mut total, &mut freed, cap).then_some((total, freed))
}

/// An upper estimate of the resident bytes a patch that applied added to the
/// content: the values `add` and `replace` insert, and `copied`, the slots
/// `add`, `copy` and `move` placed values in plus what its `copy` operations
/// duplicated (a copy costs a few bytes on the wire and a deep clone in
/// memory; the engine measured each value as it was copied, since a later
/// operation may have removed or moved its source), before the credits
/// [`patch_credit`] and [`slot_credit`] take off. The paths are not charged:
/// nothing holds them once the patch applied. Stops once the estimate
/// passes `headroom`: the caller is over its limit by then.
fn patch_bytes(operations: &[PatchOperation], copied: usize, headroom: usize) -> usize {
    let mut total = copied;
    for op in operations {
        match op.op {
            patch::PatchOp::Add | patch::PatchOp::Replace => {
                let n = op.value.as_ref().map_or(0, resident_bytes);
                total = total.saturating_add(n);
            }
            _ => {}
        }
        if total > headroom {
            break;
        }
    }
    total
}

type CallHolders = HashMap<ToolCallId, std::collections::BTreeSet<(usize, usize)>>;

/// Maps the calls of the assistant `message` (at `at`) unless an earlier
/// holder of the same call id exists (the first occurrence names it); every
/// holder of a shared id is recorded in `dups`.
fn map_calls(
    calls: &mut HashMap<ToolCallId, CallSlot>,
    dups: &mut CallHolders,
    at: usize,
    message: &Message,
) {
    let Message::Assistant(a) = message else {
        return;
    };
    for (position, call) in a.tool_calls.iter().flatten().enumerate() {
        let slot = CallSlot {
            message: at,
            position,
        };
        match calls.entry(call.id.clone()) {
            std::collections::hash_map::Entry::Vacant(v) => {
                v.insert(slot);
            }
            std::collections::hash_map::Entry::Occupied(mut o) => {
                let held = *o.get();
                dups.entry(call.id.clone())
                    .or_insert_with(|| [(held.message, held.position)].into())
                    .insert((at, position));
                if (at, position) < (held.message, held.position) {
                    o.insert(slot);
                }
            }
        }
    }
}

/// Forgets that the call at `(message, position)` holds `id`; another held
/// call with the same id takes over the mapping.
fn unmap_call(
    calls: &mut HashMap<ToolCallId, CallSlot>,
    dups: &mut CallHolders,
    id: &ToolCallId,
    message: usize,
    position: usize,
) {
    let Some(holders) = dups.get_mut(id) else {
        if calls.get(id).is_some_and(|s| s.message == message) {
            calls.remove(id);
        }
        return;
    };
    holders.remove(&(message, position));
    let Some(&(m, p)) = holders.first() else {
        dups.remove(id);
        calls.remove(id);
        return;
    };
    if holders.len() == 1 {
        dups.remove(id);
    }
    calls.insert(
        id.clone(),
        CallSlot {
            message: m,
            position: p,
        },
    );
}

/// The call at `(from, position)` moved to message `to`.
fn remap_call(
    calls: &mut HashMap<ToolCallId, CallSlot>,
    dups: &mut CallHolders,
    id: &ToolCallId,
    (from, to): (usize, usize),
    position: usize,
) {
    if let Some(holders) = dups.get_mut(id) {
        holders.remove(&(from, position));
        holders.insert((to, position));
        if let Some(&(m, p)) = holders.first() {
            calls.insert(
                id.clone(),
                CallSlot {
                    message: m,
                    position: p,
                },
            );
        }
    } else if let Some(slot) = calls.get_mut(id) {
        if slot.message == from {
            slot.message = to;
        }
    }
}

/// Per-message and per-call overhead charged against the history limit, so
/// that many empty messages count: what the reducer holds for each beyond the
/// message's own heap (its `Vec<Message>` or `Vec<ToolCall>` slot with growth
/// slack, the index entry, the run-id entry, the charge ledger slot and the
/// id copies those hold). The verifier's and the run's tracking of it are
/// charged by the bookkeeping ledgers instead (docs/DECISIONS.md D25).
/// Calibrated with a counting allocator (`tests/history_resident.rs`): about
/// 110 bytes beyond one `Message` slot.
const ITEM_OVERHEAD: usize = std::mem::size_of::<Message>() + 110;

/// Retention class of a held message, for [`Reducer::reconcile`]: 0 is any
/// role a snapshot replaces wholesale, 1 activity and 2 reasoning (the
/// client-only roles, STATE-07).
fn class(m: &Message) -> usize {
    match m {
        Message::Activity(_) => 1,
        Message::Reasoning(_) => 2,
        _ => 0,
    }
}

/// A byte count as a signed estimate.
fn signed(bytes: usize) -> i64 {
    i64::try_from(bytes).unwrap_or(i64::MAX)
}

/// What a message is estimated to retain, in resident bytes: the fixed
/// overhead for it and each call it holds, its own heap, and the part of each
/// content part's slot beyond the [`Value`] slot [`resident_bytes`] assumes
/// for every sequence element.
fn message_bytes(m: &Message) -> usize {
    let (calls, parts) = match m {
        Message::Assistant(a) => (a.tool_calls.as_ref().map_or(0, Vec::len), 0),
        Message::User(u) => (
            0,
            match &u.content {
                crate::content::Content::Parts(p) => p.len(),
                crate::content::Content::Text(_) => 0,
            },
        ),
        _ => (0, 0),
    };
    const PART_EXTRA: usize =
        std::mem::size_of::<crate::content::ContentPart>().saturating_sub(crate::json::VALUE_SLOT);
    ITEM_OVERHEAD
        .saturating_mul(calls.saturating_add(1))
        .saturating_add(resident_bytes(m))
        .saturating_add(parts.saturating_mul(PART_EXTRA))
}

fn new_text_message(opening: &TextOpening<'_>) -> Message {
    let id = opening.id.clone();
    let name = opening.name.map(str::to_owned);
    let owner = opening.owner.cloned();
    match opening.role {
        TextMessageRole::Assistant => Message::Assistant(AssistantMessage {
            id,
            content: Some(String::new()),
            tool_calls: None,
            name,
            encrypted_value: None,
            metadata: None,
            subagent_run_id: owner,
        }),
        TextMessageRole::User => Message::User(UserMessage {
            id,
            content: Content::Text(String::new()),
            name,
            encrypted_value: None,
            metadata: None,
            subagent_run_id: owner,
        }),
        TextMessageRole::System => Message::System(SystemMessage {
            id,
            content: String::new(),
            name,
            encrypted_value: None,
            metadata: None,
            subagent_run_id: owner,
        }),
        TextMessageRole::Developer => Message::Developer(DeveloperMessage {
            id,
            content: String::new(),
            name,
            encrypted_value: None,
            metadata: None,
            subagent_run_id: owner,
        }),
    }
}

fn new_call_carrier(id: MessageId, owner: Option<&SubagentRunId>) -> Message {
    Message::Assistant(AssistantMessage {
        id,
        content: None,
        tool_calls: None,
        name: None,
        encrypted_value: None,
        metadata: None,
        subagent_run_id: owner.cloned(),
    })
}

fn message_id(m: &Message) -> &MessageId {
    match m {
        Message::Developer(m) => &m.id,
        Message::System(m) => &m.id,
        Message::Assistant(m) => &m.id,
        Message::User(m) => &m.id,
        Message::Tool(m) => &m.id,
        Message::Activity(m) => &m.id,
        Message::Reasoning(m) => &m.id,
    }
}

fn message_owner(m: &Message) -> Option<&SubagentRunId> {
    match m {
        Message::Developer(m) => m.subagent_run_id.as_ref(),
        Message::System(m) => m.subagent_run_id.as_ref(),
        Message::Assistant(m) => m.subagent_run_id.as_ref(),
        Message::User(m) => m.subagent_run_id.as_ref(),
        Message::Tool(m) => m.subagent_run_id.as_ref(),
        Message::Activity(m) => m.subagent_run_id.as_ref(),
        Message::Reasoning(m) => m.subagent_run_id.as_ref(),
    }
}

fn message_name(m: &Message) -> Option<&str> {
    match m {
        Message::Developer(m) => m.name.as_deref(),
        Message::System(m) => m.name.as_deref(),
        Message::Assistant(m) => m.name.as_deref(),
        Message::User(m) => m.name.as_deref(),
        Message::Tool(_) | Message::Activity(_) | Message::Reasoning(_) => None,
    }
}

fn role_matches(m: &Message, role: TextMessageRole) -> bool {
    matches!(
        (m, role),
        (Message::Assistant(_), TextMessageRole::Assistant)
            | (Message::User(_), TextMessageRole::User)
            | (Message::System(_), TextMessageRole::System)
            | (Message::Developer(_), TextMessageRole::Developer)
    )
}

fn metadata_mut(m: &mut Message) -> &mut Option<Metadata> {
    match m {
        Message::Developer(m) => &mut m.metadata,
        Message::System(m) => &mut m.metadata,
        Message::Assistant(m) => &mut m.metadata,
        Message::User(m) => &mut m.metadata,
        Message::Tool(m) => &mut m.metadata,
        Message::Activity(m) => &mut m.metadata,
        Message::Reasoning(m) => &mut m.metadata,
    }
}

/// Where a message keeps an encrypted value; activity messages have none
/// (REAS-16).
fn encrypted_slot(m: &mut Message) -> Option<&mut Option<String>> {
    match m {
        Message::Developer(m) => Some(&mut m.encrypted_value),
        Message::System(m) => Some(&mut m.encrypted_value),
        Message::Assistant(m) => Some(&mut m.encrypted_value),
        Message::User(m) => Some(&mut m.encrypted_value),
        Message::Tool(m) => Some(&mut m.encrypted_value),
        Message::Reasoning(m) => Some(&mut m.encrypted_value),
        Message::Activity(_) => None,
    }
}

/// STRM-04: deltas concatenate in arrival order, appended in place.
fn append_text(m: &mut Message, delta: &str) {
    match m {
        Message::Developer(m) => m.content.push_str(delta),
        Message::System(m) => m.content.push_str(delta),
        Message::Assistant(m) => m.content.get_or_insert_with(String::new).push_str(delta),
        Message::Reasoning(m) => m.content.push_str(delta),
        Message::User(m) => append_content(&mut m.content, delta),
        Message::Tool(m) => append_content(&mut m.content, delta),
        Message::Activity(_) => {}
    }
}

/// Releases the growth slack of the text [`append_text`] appends to.
fn shrink_text(m: &mut Message) {
    match m {
        Message::Developer(m) => m.content.shrink_to_fit(),
        Message::System(m) => m.content.shrink_to_fit(),
        Message::Assistant(m) => {
            if let Some(text) = &mut m.content {
                text.shrink_to_fit();
            }
        }
        Message::Reasoning(m) => m.content.shrink_to_fit(),
        Message::User(m) => shrink_content(&mut m.content),
        Message::Tool(m) => shrink_content(&mut m.content),
        Message::Activity(_) => {}
    }
}

fn shrink_content(content: &mut Content) {
    match content {
        Content::Text(text) => text.shrink_to_fit(),
        Content::Parts(parts) => {
            if let Some(ContentPart::Text(part)) = parts.last_mut() {
                part.text.shrink_to_fit();
            }
        }
    }
}

fn append_content(content: &mut Content, delta: &str) {
    match content {
        Content::Text(text) => text.push_str(delta),
        Content::Parts(parts) => match parts.last_mut() {
            Some(ContentPart::Text(part)) => part.text.push_str(delta),
            _ => parts.push(ContentPart::Text(TextPart {
                text: delta.to_owned(),
                id: None,
                metadata: None,
            })),
        },
    }
}

#[cfg(test)]
mod tests;
