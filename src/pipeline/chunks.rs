//! Chunk expansion (`patterns/streaming.mdx`, "Chunk events").
//!
//! `TEXT_MESSAGE_CHUNK`, `TOOL_CALL_CHUNK` and `REASONING_MESSAGE_CHUNK` are
//! expanded into start/content/end before verification and application code
//! (STRM-07, PROC-28). Lanes are tracked per owner (parent agent or one
//! subagent invocation) and per kind:
//!
//! * the first chunk of an item must carry its opener fields (STRM-11,
//!   STRM-13, STRM-14); a missing one is a violation, never an invented id
//!   (STRM-16);
//! * a continuation repeating an opener field with a different value is fatal,
//!   including a conflict with `assistant` established by omission (STRM-20);
//! * an id-less, untagged continuation resolves to the parent's open stream of
//!   its kind, else the sole open one; otherwise it is ambiguous and fatal
//!   (STRM-23, STRM-24);
//! * `*_END` is synthesized when an event arrives in the item's lane, when a
//!   chunk names a different id, at run boundaries, at `MESSAGES_SNAPSHOT`, and
//!   at the owning subagent's terminal (STRM-18, STRM-21, STRM-22, STRM-26);
//! * chunk metadata applies to the synthesized events; a metadata-only
//!   continuation is legal (STRM-27).
//!
//! # Lanes
//!
//! A lane is an owner: the parent agent, or one `subagentRunId`. Each lane
//! holds at most one pending chunk stream, of any kind. The closing table:
//!
//! | Event | Closes |
//! |---|---|
//! | `RUN_STARTED`, `RUN_FINISHED`, `RUN_ERROR`, `MESSAGES_SNAPSHOT` | every lane |
//! | `SUBAGENT_FINISHED`, `SUBAGENT_ERROR` | that subagent's lane |
//! | `RAW`, `ACTIVITY_*`, `REASONING_ENCRYPTED_VALUE`, `SUBAGENT_STARTED` | nothing |
//! | a chunk naming a different item | its own lane |
//! | every other event (messages, tool calls and results, steps, state, `CUSTOM`, reasoning) | its own lane |
//!
//! An explicit event's own lane is the lane assembling the item it names when
//! one is (an explicit event naming an assembling id ends the shorthand,
//! STRM-10), and otherwise the lane of its `subagentRunId` (the parent's when
//! untagged).
//!
//! What a chunk carries is moved onto the synthesized events: deltas, opener
//! fields, metadata (on every event synthesized from the chunk), `rawEvent`
//! (on the first) and attribution. Its `timestamp` is not: the synthesized
//! events are not the chunk (PROC-25).
//!
//! Cost: O(1) per event. The parent lane is a slot, subagent lanes are a hash
//! map keyed by the invocation id, and three per-kind indexes (keyed by the
//! item id's shared `Arc<str>`) find the lane assembling a named item. Ids are
//! reference-counted clones, so a continuation chunk allocates nothing.

use std::collections::HashMap;
use std::sync::Arc;

use crate::diagnostics::shown;
use crate::error::{Error, ErrorCode, Result};
use crate::event::Event;
use crate::ids::{MessageId, SubagentRunId, ToolCallId};
use crate::metadata::Metadata;
use crate::reasoning::{
    ReasoningMessageChunk, ReasoningMessageContent, ReasoningMessageEnd, ReasoningMessageStart,
    ReasoningRole,
};
use crate::text::{
    TextMessageChunk, TextMessageContent, TextMessageEnd, TextMessageRole, TextMessageStart,
};
use crate::tool::{ToolCallArgs, ToolCallChunk, ToolCallEnd, ToolCallStart};

/// Who owns a lane: `None` is the parent agent.
type Owner = Option<SubagentRunId>;

/// The chunk expansion stage.
#[derive(Debug, Default)]
pub struct ChunkExpander {
    /// The parent agent's pending chunk stream.
    parent: Option<Lane>,
    /// Pending chunk streams of subagent lanes. Only lanes with a pending
    /// stream have an entry.
    subagents: HashMap<SubagentRunId, Lane>,
    /// Per kind ([`Kind::index`]): item id → owner of the lane assembling it.
    by_item: [HashMap<Arc<str>, Owner>; 3],
    /// Opening order, so lanes close deterministically.
    next_seq: u64,
    /// Scratch for [`close_all`](Self::close_all); capacity is reused.
    closing: Vec<(u64, Owner)>,
}

/// The kinds of chunk stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Text,
    Tool,
    Reasoning,
}

impl Kind {
    const fn index(self) -> usize {
        match self {
            Kind::Text => 0,
            Kind::Tool => 1,
            Kind::Reasoning => 2,
        }
    }

    const fn chunk_type(self) -> &'static str {
        match self {
            Kind::Text => "TEXT_MESSAGE_CHUNK",
            Kind::Tool => "TOOL_CALL_CHUNK",
            Kind::Reasoning => "REASONING_MESSAGE_CHUNK",
        }
    }

    /// The violation for a chunk that would open a stream without its id
    /// (STRM-16: an id is never invented).
    fn missing_id(self) -> Error {
        chunk_error(
            match self {
                // Wording pinned by the conformance corpus.
                Kind::Text => "First TEXT_MESSAGE_CHUNK must have a messageId",
                Kind::Tool => "TOOL_CALL_CHUNK cannot open a tool call: toolCallId is absent",
                Kind::Reasoning => {
                    "REASONING_MESSAGE_CHUNK cannot open a reasoning message: messageId is absent"
                }
            }
            .to_owned(),
        )
    }

    const fn id_field(self) -> &'static str {
        match self {
            Kind::Tool => "toolCallId",
            Kind::Text | Kind::Reasoning => "messageId",
        }
    }
}

/// What a subagent's pending chunk stream is estimated to hold beyond the
/// ids its item shares with the verifier: its slot in the subagent lane
/// table and its `by_item` entry. A lane exists only while its item is
/// open, so the consumer verifier charges this with each subagent-owned
/// text, tool-call and reasoning item it tracks; the parent agent has one
/// lane at most (docs/DECISIONS.md D25).
pub(crate) const LANE_SHARE: usize = super::table_slot(16 + std::mem::size_of::<Lane>())
    + super::table_slot(16 + std::mem::size_of::<Owner>());

/// One pending chunk stream: the item it assembles and the opener fields
/// later chunks are checked against.
#[derive(Debug)]
struct Lane {
    item: Item,
    owner: Owner,
    seq: u64,
}

#[derive(Debug)]
enum Item {
    Text {
        id: MessageId,
        /// Effective role: an absent role on the first chunk is `assistant`.
        role: TextMessageRole,
        /// Fingerprint of the opener's `name`, if it had one.
        name: Option<Fingerprint>,
    },
    Tool {
        id: ToolCallId,
        /// The opener's `toolCallName`, fingerprinted.
        name: Fingerprint,
        /// Fingerprint of the opener's `parentMessageId`, if it had one. A
        /// copy would be a peer string the lane alone keeps (the history
        /// does not share this `Arc`), uncharged, once per subagent lane
        /// (docs/DECISIONS.md D25).
        parent: Option<Fingerprint>,
    },
    Reasoning {
        id: MessageId,
    },
}

impl Item {
    fn kind(&self) -> Kind {
        match self {
            Item::Text { .. } => Kind::Text,
            Item::Tool { .. } => Kind::Tool,
            Item::Reasoning { .. } => Kind::Reasoning,
        }
    }

    fn key(&self) -> &Arc<str> {
        match self {
            Item::Text { id, .. } | Item::Reasoning { id } => id.as_arc(),
            Item::Tool { id, .. } => id.as_arc(),
        }
    }
}

/// A 64-bit fingerprint of an opener's string field, kept instead of a copy
/// so that opening an item allocates nothing (the string itself moves into
/// the synthesized start event). Conflicting repeats are found by comparing
/// fingerprints ([`super::fingerprint`]), whose keys are random per process,
/// so a peer cannot craft two different values that compare equal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Fingerprint(u64);

impl Fingerprint {
    fn of(text: &str) -> Self {
        Fingerprint(super::fingerprint(text))
    }
}

/// Where a chunk goes, once resolved.
enum Target {
    /// Continue the pending stream of this lane.
    Continue(Owner),
    /// Open a new item in this lane (closing whatever it holds).
    Open(Owner),
}

fn role_str(role: TextMessageRole) -> &'static str {
    match role {
        TextMessageRole::Developer => "developer",
        TextMessageRole::System => "system",
        TextMessageRole::Assistant => "assistant",
        TextMessageRole::User => "user",
    }
}

fn chunk_error(message: String) -> Error {
    Error::new(ErrorCode::Chunk, message)
}

fn owner_text(owner: &Owner) -> String {
    owner
        .as_ref()
        .map_or_else(|| "the parent agent".to_owned(), |id| shown(id.as_str()))
}

/// Which pending chunk streams an explicit event ends (the closing table in
/// the module docs).
enum Reach {
    /// Run boundaries and snapshots end every lane.
    Everything,
    /// A subagent terminal ends only that invocation's lane.
    Subagent(SubagentRunId),
    /// Side-channel events leave all lanes untouched.
    Nothing,
    /// Anything else ends the lane it belongs to.
    OwnLane,
}

impl Reach {
    fn of(event: &Event) -> Self {
        match event {
            Event::RunStarted(_)
            | Event::RunFinished(_)
            | Event::RunError(_)
            | Event::MessagesSnapshot(_) => Self::Everything,
            Event::SubagentFinished(done) => Self::Subagent(done.subagent_run_id.clone()),
            Event::SubagentError(failed) => Self::Subagent(failed.subagent_run_id.clone()),
            Event::Raw(_)
            | Event::ActivitySnapshot(_)
            | Event::ActivityDelta(_)
            | Event::ReasoningEncryptedValue(_)
            | Event::SubagentStarted(_) => Self::Nothing,
            _ => Self::OwnLane,
        }
    }
}

impl ChunkExpander {
    /// An expander for one stream.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Expands one enforced event, appending the result to `out` (the event
    /// itself if it is not a chunk, preceded by any synthesized `*_END`).
    ///
    /// # Errors
    ///
    /// [`ErrorCode::Chunk`] for the fatal cases listed in the [module docs](crate::pipeline::chunks),
    /// and [`ErrorCode::Ownership`] for a chunk
    /// whose `subagentRunId` disagrees with the lane assembling its item.
    pub fn push(&mut self, event: Event, out: &mut Vec<Event>) -> Result<()> {
        let explicit = match event {
            Event::TextMessageChunk(text) => return self.text_chunk(text, out),
            Event::ToolCallChunk(call) => return self.tool_chunk(call, out),
            Event::ReasoningMessageChunk(thought) => return self.reasoning_chunk(thought, out),
            other => other,
        };
        match Reach::of(&explicit) {
            Reach::Everything => self.close_all(out),
            Reach::Subagent(id) => self.close_lane(&Some(id), out),
            Reach::OwnLane => self.close_own_lane(&explicit, out),
            Reach::Nothing => {}
        }
        out.push(explicit);
        Ok(())
    }

    /// Closes every open chunk stream (stream end), appending synthesized ends.
    pub fn close_all(&mut self, out: &mut Vec<Event>) {
        if self.parent.is_none() && self.subagents.is_empty() {
            return;
        }
        let mut closing = std::mem::take(&mut self.closing);
        closing.clear();
        if let Some(lane) = &self.parent {
            closing.push((lane.seq, None));
        }
        for (id, lane) in &self.subagents {
            closing.push((lane.seq, Some(id.clone())));
        }
        closing.sort_unstable_by_key(|(seq, _)| *seq);
        for (_, owner) in closing.drain(..) {
            self.close_lane(&owner, out);
        }
        self.closing = closing;
    }

    /// Whether any chunk stream is pending.
    pub(crate) fn is_idle(&self) -> bool {
        self.parent.is_none() && self.subagents.is_empty()
    }

    fn lane(&self, owner: &Owner) -> Option<&Lane> {
        match owner {
            None => self.parent.as_ref(),
            Some(id) => self.subagents.get(id),
        }
    }

    /// Ends the pending stream of `owner`'s lane, if any.
    fn close_lane(&mut self, owner: &Owner, out: &mut Vec<Event>) {
        let lane = match owner {
            None => self.parent.take(),
            Some(id) => self.subagents.remove(id),
        };
        let Some(lane) = lane else { return };
        self.by_item[lane.item.kind().index()].remove(lane.item.key());
        let owner = lane.owner;
        out.push(match lane.item {
            Item::Text { id, .. } => Event::TextMessageEnd(TextMessageEnd {
                message_id: id,
                timestamp: None,
                raw_event: None,
                metadata: None,
                subagent_run_id: owner,
            }),
            Item::Tool { id, .. } => Event::ToolCallEnd(ToolCallEnd {
                tool_call_id: id,
                timestamp: None,
                raw_event: None,
                metadata: None,
                subagent_run_id: owner,
            }),
            Item::Reasoning { id } => Event::ReasoningMessageEnd(ReasoningMessageEnd {
                message_id: id,
                timestamp: None,
                raw_event: None,
                metadata: None,
                subagent_run_id: owner,
            }),
        });
    }

    fn open_lane(&mut self, owner: Owner, item: Item) {
        let seq = self.next_seq;
        self.next_seq += 1;
        self.by_item[item.kind().index()].insert(item.key().clone(), owner.clone());
        let lane = Lane {
            item,
            owner: owner.clone(),
            seq,
        };
        match owner {
            None => self.parent = Some(lane),
            Some(id) => {
                self.subagents.insert(id, lane);
            }
        }
    }

    /// The lane an explicit (non-chunk) event belongs to, for closing.
    fn close_own_lane(&mut self, event: &Event, out: &mut Vec<Event>) {
        if self.is_idle() {
            return;
        }
        let named: Option<(Kind, &str)> = match event {
            Event::TextMessageStart(e) => Some((Kind::Text, e.message_id.as_str())),
            Event::TextMessageContent(e) => Some((Kind::Text, e.message_id.as_str())),
            Event::TextMessageEnd(e) => Some((Kind::Text, e.message_id.as_str())),
            Event::ToolCallStart(e) => Some((Kind::Tool, e.tool_call_id.as_str())),
            Event::ToolCallArgs(e) => Some((Kind::Tool, e.tool_call_id.as_str())),
            Event::ToolCallEnd(e) => Some((Kind::Tool, e.tool_call_id.as_str())),
            Event::ToolCallResult(e) => Some((Kind::Tool, e.tool_call_id.as_str())),
            Event::ReasoningMessageStart(e) => Some((Kind::Reasoning, e.message_id.as_str())),
            Event::ReasoningMessageContent(e) => Some((Kind::Reasoning, e.message_id.as_str())),
            Event::ReasoningMessageEnd(e) => Some((Kind::Reasoning, e.message_id.as_str())),
            _ => None,
        };
        let assembling = named.and_then(|(kind, id)| self.by_item[kind.index()].get(id).cloned());
        let owner = match assembling {
            Some(owner) => owner,
            None => event.subagent_run_id().cloned(),
        };
        self.close_lane(&owner, out);
    }

    /// Decides which lane a chunk continues or opens.
    ///
    /// `id` is the item id the chunk names, `tag` its `subagentRunId`.
    fn resolve(&self, kind: Kind, id: Option<&str>, tag: &Owner) -> Result<Target> {
        if let Some(id) = id {
            return match self.by_item[kind.index()].get(id) {
                Some(owner) => {
                    if tag.is_some() && tag != owner {
                        Err(Error::new(
                            ErrorCode::Ownership,
                            format!(
                                "{} for {} '{}' carries subagentRunId '{}', which does not match \
                                 the owner of the open stream ({})",
                                kind.chunk_type(),
                                kind.id_field(),
                                shown(id),
                                owner_text(tag),
                                owner_text(owner),
                            ),
                        ))
                    } else {
                        Ok(Target::Continue(owner.clone()))
                    }
                }
                None => Ok(Target::Open(tag.clone())),
            };
        }
        // No id: a continuation, resolved by attribution (STRM-23, STRM-24).
        let of_kind = |owner: &Owner| {
            self.lane(owner)
                .is_some_and(|lane| lane.item.kind() == kind)
        };
        if tag.is_some() {
            return if of_kind(tag) {
                Ok(Target::Continue(tag.clone()))
            } else {
                Err(kind.missing_id())
            };
        }
        if of_kind(&None) {
            return Ok(Target::Continue(None));
        }
        let index = &self.by_item[kind.index()];
        match index.len() {
            0 => Err(kind.missing_id()),
            1 => Ok(Target::Continue(
                index.values().next().cloned().unwrap_or_default(),
            )),
            open => Err(chunk_error(format!(
                "{} carries neither a {} nor a subagentRunId, and {} subagent lanes hold open \
                 streams of its kind: the continuation is ambiguous",
                kind.chunk_type(),
                kind.id_field(),
                open
            ))),
        }
    }

    fn text_chunk(&mut self, chunk: TextMessageChunk, out: &mut Vec<Event>) -> Result<()> {
        let TextMessageChunk {
            message_id,
            role,
            delta,
            name,
            timestamp: _,
            raw_event,
            metadata,
            subagent_run_id: tag,
        } = chunk;
        match self.resolve(Kind::Text, message_id.as_ref().map(|id| id.as_str()), &tag)? {
            Target::Open(owner) => {
                let Some(id) = message_id else {
                    return Err(Kind::Text.missing_id());
                };
                self.close_lane(&owner, out);
                let fingerprint = name.as_deref().map(Fingerprint::of);
                out.push(Event::TextMessageStart(TextMessageStart {
                    message_id: id.clone(),
                    role,
                    name,
                    timestamp: None,
                    raw_event,
                    metadata: metadata.clone(),
                    subagent_run_id: tag.clone(),
                }));
                if let Some(delta) = delta {
                    out.push(Event::TextMessageContent(TextMessageContent {
                        message_id: id.clone(),
                        delta,
                        timestamp: None,
                        raw_event: None,
                        metadata,
                        subagent_run_id: tag,
                    }));
                }
                self.open_lane(
                    owner,
                    Item::Text {
                        id,
                        role: role.unwrap_or(TextMessageRole::Assistant),
                        name: fingerprint,
                    },
                );
                Ok(())
            }
            Target::Continue(owner) => {
                let Some(lane) = self.lane(&owner) else {
                    return Err(Error::new(ErrorCode::Internal, "chunk lane vanished"));
                };
                let Item::Text {
                    id,
                    role: open_role,
                    name: open_name,
                } = &lane.item
                else {
                    return Err(Error::new(ErrorCode::Internal, "chunk lane kind mismatch"));
                };
                if let Some(role) = role {
                    if role != *open_role {
                        return Err(chunk_error(format!(
                            "TEXT_MESSAGE_CHUNK for messageId '{}' has role '{}', which does not \
                             match the open stream's role '{}'",
                            shown(id.as_str()),
                            role_str(role),
                            role_str(*open_role)
                        )));
                    }
                }
                if let Some(name) = &name {
                    if *open_name != Some(Fingerprint::of(name)) {
                        return Err(chunk_error(format!(
                            "TEXT_MESSAGE_CHUNK for messageId '{}' has a name that does not \
                             match the open stream's name",
                            shown(id.as_str())
                        )));
                    }
                }
                let id = id.clone();
                push_text_content(id, delta, raw_event, metadata, tag, out);
                Ok(())
            }
        }
    }

    fn tool_chunk(&mut self, chunk: ToolCallChunk, out: &mut Vec<Event>) -> Result<()> {
        let ToolCallChunk {
            tool_call_id,
            tool_call_name,
            parent_message_id,
            delta,
            timestamp: _,
            raw_event,
            metadata,
            subagent_run_id: tag,
        } = chunk;
        match self.resolve(
            Kind::Tool,
            tool_call_id.as_ref().map(|id| id.as_str()),
            &tag,
        )? {
            Target::Open(owner) => {
                let Some(id) = tool_call_id else {
                    return Err(Kind::Tool.missing_id());
                };
                let Some(name) = tool_call_name else {
                    return Err(chunk_error(format!(
                        "First TOOL_CALL_CHUNK must have a toolCallName (toolCallId '{}')",
                        shown(id.as_str())
                    )));
                };
                self.close_lane(&owner, out);
                let fingerprint = Fingerprint::of(&name);
                let parent = parent_message_id
                    .as_ref()
                    .map(|id| Fingerprint::of(id.as_str()));
                out.push(Event::ToolCallStart(ToolCallStart {
                    tool_call_id: id.clone(),
                    tool_call_name: name,
                    parent_message_id,
                    timestamp: None,
                    raw_event,
                    metadata: metadata.clone(),
                    subagent_run_id: tag.clone(),
                }));
                if let Some(delta) = delta {
                    out.push(Event::ToolCallArgs(ToolCallArgs {
                        tool_call_id: id.clone(),
                        delta,
                        timestamp: None,
                        raw_event: None,
                        metadata,
                        subagent_run_id: tag,
                    }));
                }
                self.open_lane(
                    owner,
                    Item::Tool {
                        id,
                        name: fingerprint,
                        parent,
                    },
                );
                Ok(())
            }
            Target::Continue(owner) => {
                let Some(lane) = self.lane(&owner) else {
                    return Err(Error::new(ErrorCode::Internal, "chunk lane vanished"));
                };
                let Item::Tool {
                    id,
                    name: open_name,
                    parent: open_parent,
                } = &lane.item
                else {
                    return Err(Error::new(ErrorCode::Internal, "chunk lane kind mismatch"));
                };
                if let Some(name) = &tool_call_name {
                    if Fingerprint::of(name) != *open_name {
                        return Err(chunk_error(format!(
                            "TOOL_CALL_CHUNK for toolCallId '{}' has a toolCallName that \
                             does not match the open stream's toolCallName",
                            shown(id.as_str())
                        )));
                    }
                }
                if let Some(parent) = &parent_message_id {
                    if *open_parent != Some(Fingerprint::of(parent.as_str())) {
                        return Err(chunk_error(format!(
                            "TOOL_CALL_CHUNK for toolCallId '{}' has parentMessageId '{}', which \
                             does not match the open stream's parentMessageId",
                            shown(id.as_str()),
                            shown(parent.as_str())
                        )));
                    }
                }
                let id = id.clone();
                if delta.is_some() || metadata.is_some() || raw_event.is_some() {
                    out.push(Event::ToolCallArgs(ToolCallArgs {
                        tool_call_id: id,
                        delta: delta.unwrap_or_default(),
                        timestamp: None,
                        raw_event,
                        metadata,
                        subagent_run_id: tag,
                    }));
                }
                Ok(())
            }
        }
    }

    fn reasoning_chunk(
        &mut self,
        chunk: ReasoningMessageChunk,
        out: &mut Vec<Event>,
    ) -> Result<()> {
        let ReasoningMessageChunk {
            message_id,
            delta,
            timestamp: _,
            raw_event,
            metadata,
            subagent_run_id: tag,
        } = chunk;
        match self.resolve(
            Kind::Reasoning,
            message_id.as_ref().map(|id| id.as_str()),
            &tag,
        )? {
            Target::Open(owner) => {
                let Some(id) = message_id else {
                    return Err(Kind::Reasoning.missing_id());
                };
                self.close_lane(&owner, out);
                out.push(Event::ReasoningMessageStart(ReasoningMessageStart {
                    message_id: id.clone(),
                    role: ReasoningRole::Reasoning,
                    timestamp: None,
                    raw_event,
                    metadata: metadata.clone(),
                    subagent_run_id: tag.clone(),
                }));
                if let Some(delta) = delta {
                    out.push(Event::ReasoningMessageContent(ReasoningMessageContent {
                        message_id: id.clone(),
                        delta,
                        timestamp: None,
                        raw_event: None,
                        metadata,
                        subagent_run_id: tag,
                    }));
                }
                self.open_lane(owner, Item::Reasoning { id });
                Ok(())
            }
            Target::Continue(owner) => {
                let Some(lane) = self.lane(&owner) else {
                    return Err(Error::new(ErrorCode::Internal, "chunk lane vanished"));
                };
                let Item::Reasoning { id } = &lane.item else {
                    return Err(Error::new(ErrorCode::Internal, "chunk lane kind mismatch"));
                };
                let id = id.clone();
                if delta.is_some() || metadata.is_some() || raw_event.is_some() {
                    out.push(Event::ReasoningMessageContent(ReasoningMessageContent {
                        message_id: id,
                        delta: delta.unwrap_or_default(),
                        timestamp: None,
                        raw_event,
                        metadata,
                        subagent_run_id: tag,
                    }));
                }
                Ok(())
            }
        }
    }
}

/// A continuation's content event. A chunk carrying only metadata (or only
/// `rawEvent`) still yields one, with an empty delta, so its metadata reaches
/// the item (STRM-27); a chunk carrying nothing at all yields nothing.
fn push_text_content(
    id: MessageId,
    delta: Option<String>,
    raw_event: Option<serde_json::Value>,
    metadata: Option<Metadata>,
    tag: Owner,
    out: &mut Vec<Event>,
) {
    if delta.is_none() && metadata.is_none() && raw_event.is_none() {
        return;
    }
    out.push(Event::TextMessageContent(TextMessageContent {
        message_id: id,
        delta: delta.unwrap_or_default(),
        timestamp: None,
        raw_event,
        metadata,
        subagent_run_id: tag,
    }));
}

#[cfg(test)]
mod tests;
