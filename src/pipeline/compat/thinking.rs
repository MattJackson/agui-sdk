//! The retired `THINKING_*` events translated to the reasoning family.
//!
//! | 0.x | 1.0 |
//! |---|---|
//! | `THINKING_START` (`title`?) | `REASONING_START` (`messageId`) |
//! | `THINKING_TEXT_MESSAGE_START` | `REASONING_MESSAGE_START` (`messageId`, `role: "reasoning"`) |
//! | `THINKING_TEXT_MESSAGE_CONTENT` (`delta`) | `REASONING_MESSAGE_CONTENT` (`messageId`, `delta`) |
//! | `THINKING_TEXT_MESSAGE_END` | `REASONING_MESSAGE_END` (`messageId`) |
//! | `THINKING_END` | `REASONING_END` (`messageId`) |
//!
//! 0.x thinking was positional: a span and a message were open or not, and
//! nothing named them. 1.0 matches by `messageId`, so the translator names
//! each span and message it sees opened, `thinking-<run>-<n>`, where `<run>`
//! is a fixed hash of the current `runId` (keeping names unique across the
//! runs of a thread without copying the run id) and `<n>` counts per stream.
//! Content and closing events take the name of what is open. A `messageId` a
//! frame already carries is kept and tracked instead.
//!
//! The spans the translator tracks are the ones still open on the stream:
//! a span closes when `THINKING_END` names it or is the latest, and also
//! when a 1.0 `REASONING_END` names it (the boundary sees that frame, though
//! it translates nothing in it), so mixing the two families leaves nothing
//! behind. An id opened again while open is one entry. What the table holds
//! is estimated like the verifier's id bookkeeping and charged to the
//! history budget by the processor that owns the boundary (D25, D28).
//!
//! Nothing is repaired: a fragment or a close with nothing open gets no
//! `messageId`, so enforcement rejects it as the malformed event it is, as it
//! would a 1.0 one. The span's `title` has no 1.0 field; dropping it loses
//! content and always warns (VER-15).

use std::collections::{BTreeMap, HashMap};
use std::hash::{Hash, Hasher};
use std::sync::Arc;

use serde_json::{Map, Value};

use super::{Notes, Notice, Path};
use crate::diagnostics::DiagnosticCode;
use crate::event::EventType;

/// One retired event. The discriminant offsets the notice bit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub(crate) enum Step {
    Start,
    End,
    MessageStart,
    MessageContent,
    MessageEnd,
}

impl Step {
    pub(crate) const ALL: &'static [Step] = &[
        Step::Start,
        Step::End,
        Step::MessageStart,
        Step::MessageContent,
        Step::MessageEnd,
    ];

    pub(crate) const COUNT: u32 = Step::ALL.len() as u32;

    pub(crate) fn of(name: &str) -> Option<Self> {
        Some(match name {
            "THINKING_START" => Step::Start,
            "THINKING_END" => Step::End,
            "THINKING_TEXT_MESSAGE_START" => Step::MessageStart,
            "THINKING_TEXT_MESSAGE_CONTENT" => Step::MessageContent,
            "THINKING_TEXT_MESSAGE_END" => Step::MessageEnd,
            _ => return None,
        })
    }

    pub(crate) fn retired_name(self) -> &'static str {
        match self {
            Step::Start => "THINKING_START",
            Step::End => "THINKING_END",
            Step::MessageStart => "THINKING_TEXT_MESSAGE_START",
            Step::MessageContent => "THINKING_TEXT_MESSAGE_CONTENT",
            Step::MessageEnd => "THINKING_TEXT_MESSAGE_END",
        }
    }

    pub(crate) fn replacement_type(self) -> EventType {
        match self {
            Step::Start => EventType::ReasoningStart,
            Step::End => EventType::ReasoningEnd,
            Step::MessageStart => EventType::ReasoningMessageStart,
            Step::MessageContent => EventType::ReasoningMessageContent,
            Step::MessageEnd => EventType::ReasoningMessageEnd,
        }
    }

    fn replacement(self) -> &'static str {
        match self {
            Step::Start => "REASONING_START",
            Step::End => "REASONING_END",
            Step::MessageStart => "REASONING_MESSAGE_START",
            Step::MessageContent => "REASONING_MESSAGE_CONTENT",
            Step::MessageEnd => "REASONING_MESSAGE_END",
        }
    }
}

/// A tracked span's estimated resident bytes, its id's heap aside: its
/// entry in `open` (a B-tree leaf entry, counted at twice its size for the
/// leaf's fill, like a hash-table slot) and in `by_id` (D25).
const SPAN_SLOT: usize = crate::pipeline::table_slot(8 + 16) * 2;

/// What one tracked span of an id of `len` bytes is charged: both entries
/// share one `Arc`, so the id's heap is charged once.
fn span_bytes(len: usize) -> usize {
    SPAN_SLOT + crate::pipeline::id_heap(len)
}

/// What the translator has opened in the current run.
#[derive(Debug, Default)]
pub(super) struct Thinking {
    run_tag: u64,
    named: u64,
    /// Open spans by opening order (`seq`), live ones only.
    open: BTreeMap<u64, Arc<str>>,
    /// The sequence number of each open id: one per id, sharing its `Arc`.
    by_id: HashMap<Arc<str>, u64>,
    seq: u64,
    message: Option<String>,
    /// Estimated resident bytes of `open`, `by_id` and `message` (D25).
    bytes: usize,
}

/// What a frame's `messageId` member holds, borrowed from the frame.
enum Id<'a> {
    Absent,
    Named(&'a str),
    /// Present but not a string: malformed, left for enforcement.
    Malformed,
}

fn message_id(map: &Map<String, Value>) -> Id<'_> {
    match map.get("messageId") {
        None => Id::Absent,
        Some(Value::String(id)) => Id::Named(id),
        Some(_) => Id::Malformed,
    }
}

impl Thinking {
    /// A new run: nothing is open, and new names use its id.
    pub(super) fn run_started(&mut self, run_id: Option<&str>) {
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        run_id.hash(&mut hasher);
        self.run_tag = hasher.finish();
        self.open.clear();
        crate::pipeline::reset_map(&mut self.by_id);
        self.message = None;
        self.bytes = 0;
    }

    /// Estimated resident bytes of what the translator keeps under
    /// peer-chosen ids (D25).
    pub(super) fn retained(&self) -> usize {
        self.bytes
    }

    /// Whether any span is tracked, so a 1.0 `REASONING_END` may close one.
    pub(super) fn has_open_spans(&self) -> bool {
        !self.open.is_empty()
    }

    /// Tracks `id` as open. An id already open stays one entry: the stream
    /// opening it again is the verifier's to judge, and the shim needs only
    /// to know it is open.
    fn open_span(&mut self, id: &str) {
        if self.by_id.contains_key(id) {
            return;
        }
        self.seq += 1;
        let id: Arc<str> = Arc::from(id);
        self.bytes = self.bytes.saturating_add(span_bytes(id.len()));
        self.open.insert(self.seq, Arc::clone(&id));
        self.by_id.insert(id, self.seq);
    }

    /// Closes the open span named `id`, if any: a `THINKING_END` naming it,
    /// or a 1.0 `REASONING_END` passing the boundary.
    pub(super) fn close_named(&mut self, id: &str) {
        if let Some(seq) = self.by_id.remove(id) {
            self.open.remove(&seq);
            self.bytes = self.bytes.saturating_sub(span_bytes(id.len()));
        }
    }

    /// Closes the most recent open span of any id, returning its id.
    fn close_latest(&mut self) -> Option<Arc<str>> {
        let (_, id) = self.open.pop_last()?;
        self.by_id.remove(&id);
        self.bytes = self.bytes.saturating_sub(span_bytes(id.len()));
        Some(id)
    }

    /// Replaces the open message, charging its id's text; the message it
    /// replaced.
    fn set_message(&mut self, id: Option<String>) -> Option<String> {
        let len = |m: &Option<String>| m.as_ref().map_or(0, |m| crate::json::text_bytes(m.len()));
        self.bytes = self
            .bytes
            .saturating_sub(len(&self.message))
            .saturating_add(len(&id));
        std::mem::replace(&mut self.message, id)
    }

    fn name(&mut self) -> String {
        self.named += 1;
        format!("thinking-{:016x}-{}", self.run_tag, self.named)
    }

    /// Rewrites one retired frame as its replacement.
    pub(super) fn translate(
        &mut self,
        step: Step,
        map: &mut Map<String, Value>,
        notes: &mut Notes<'_, '_>,
    ) {
        map.insert(
            "type".to_owned(),
            Value::String(step.replacement().to_owned()),
        );
        notes.notice(
            Notice::Thinking(step),
            DiagnosticCode::RetiredShape,
            &Path::ROOT,
            || {
                format!(
                    "retired {} translated to {}; update the producer to emit REASONING_* \
                     events (shim expires {})",
                    step.retired_name(),
                    step.replacement(),
                    super::SHIM_EXPIRY
                )
            },
        );
        match step {
            Step::Start => {
                self.drop_title(map, notes);
                match message_id(map) {
                    Id::Named(id) => self.open_span(id),
                    Id::Absent => {
                        let id = self.name();
                        self.open_span(&id);
                        map.insert("messageId".to_owned(), Value::String(id));
                    }
                    Id::Malformed => {}
                }
            }
            Step::End => match message_id(map) {
                Id::Named(id) => {
                    self.close_named(id);
                }
                Id::Absent => {
                    if let Some(id) = self.close_latest() {
                        map.insert("messageId".to_owned(), Value::String(id.to_string()));
                    }
                }
                Id::Malformed => {}
            },
            Step::MessageStart => {
                match message_id(map) {
                    Id::Named(id) => {
                        self.set_message(Some(id.to_owned()));
                    }
                    Id::Absent => {
                        let id = self.name();
                        map.insert("messageId".to_owned(), Value::String(id.clone()));
                        self.set_message(Some(id));
                    }
                    Id::Malformed => {}
                }
                if !map.contains_key("role") {
                    map.insert("role".to_owned(), Value::String("reasoning".to_owned()));
                }
            }
            Step::MessageContent => {
                if let (Id::Absent, Some(id)) = (message_id(map), &self.message) {
                    map.insert("messageId".to_owned(), Value::String(id.clone()));
                }
            }
            Step::MessageEnd => match message_id(map) {
                Id::Named(id) => {
                    if self.message.as_deref() == Some(id) {
                        self.set_message(None);
                    }
                }
                Id::Absent => {
                    if let Some(id) = self.set_message(None) {
                        map.insert("messageId".to_owned(), Value::String(id));
                    }
                }
                Id::Malformed => {}
            },
        }
    }

    fn drop_title(&self, map: &mut Map<String, Value>, notes: &mut Notes<'_, '_>) {
        match super::remove_key(map, "title") {
            None | Some(Value::Null) => {}
            Some(_) => notes.lossy(
                DiagnosticCode::RetiredShape,
                &Path::ROOT.key("title"),
                || {
                    "THINKING_START `title` dropped: REASONING_START has no field for it; update \
                 the producer to emit REASONING_* events"
                        .to_owned()
                },
            ),
        }
    }
}

#[cfg(test)]
mod span_tests {
    use super::*;
    use crate::diagnostics::{NullSink, Reporter};
    use crate::pipeline::compat::Boundary;
    use serde_json::json;
    use std::sync::Arc;

    fn run(frames: Vec<Value>) -> Vec<Value> {
        let mut boundary = Boundary::new();
        let mut reporter = Reporter::new(Arc::new(NullSink), 0);
        frames
            .into_iter()
            .map(|mut f| {
                boundary.upgrade(&mut f, &mut reporter);
                f
            })
            .collect()
    }

    #[test]
    fn ends_close_the_named_or_latest_span() {
        let out = run(vec![
            json!({"type": "RUN_STARTED", "threadId": "t", "runId": "r"}),
            json!({"type": "THINKING_START", "messageId": "a"}),
            json!({"type": "THINKING_START"}),
            // `a` is open already: still one entry, closed by one end.
            json!({"type": "THINKING_START", "messageId": "a"}),
            json!({"type": "THINKING_END", "messageId": "a"}),
            json!({"type": "THINKING_END"}),
            json!({"type": "THINKING_END"}),
            json!({"type": "THINKING_START", "messageId": "z"}),
            json!({"type": "RUN_STARTED", "threadId": "t", "runId": "r2"}),
            json!({"type": "THINKING_END"}),
        ]);
        let generated = out[2]["messageId"].as_str().unwrap_or_default().to_owned();
        assert!(!generated.is_empty());
        assert_eq!(out[5]["messageId"], generated.as_str());
        assert!(out[6].get("messageId").is_none());
        assert!(out[9].get("messageId").is_none());
    }

    /// A 1.0 `REASONING_END` closes the span in the shim too, and the
    /// shim's charge returns to zero when nothing is open.
    #[test]
    fn a_reasoning_end_closes_a_translated_span() {
        let mut boundary = Boundary::new();
        let mut reporter = Reporter::new(Arc::new(NullSink), 0);
        let mut feed = |boundary: &mut Boundary, mut f: Value| {
            boundary.upgrade(&mut f, &mut reporter);
            f
        };
        feed(
            &mut boundary,
            json!({"type": "RUN_STARTED", "threadId": "t", "runId": "r"}),
        );
        feed(&mut boundary, json!({"type": "THINKING_START"}));
        feed(
            &mut boundary,
            json!({"type": "THINKING_START", "messageId": "x"}),
        );
        assert!(boundary.retained() > 0);
        feed(
            &mut boundary,
            json!({"type": "REASONING_END", "messageId": "x"}),
        );
        // The latest open span is the unnamed one again.
        let end = feed(&mut boundary, json!({"type": "THINKING_END"}));
        assert!(end["messageId"]
            .as_str()
            .is_some_and(|id| id.starts_with("thinking-")));
        assert!(!boundary.tracks_spans());
        assert_eq!(boundary.retained(), 0);
        feed(
            &mut boundary,
            json!({"type": "THINKING_TEXT_MESSAGE_START", "messageId": "m"}),
        );
        assert!(boundary.retained() > 0);
        feed(&mut boundary, json!({"type": "THINKING_TEXT_MESSAGE_END"}));
        assert_eq!(boundary.retained(), 0);
    }

    #[test]
    fn an_oversized_span_table_is_dropped_after_a_small_run() {
        let mut boundary = Boundary::new();
        let mut reporter = Reporter::new(Arc::new(NullSink), 0);
        let mut feed = |mut f: Value| boundary.upgrade(&mut f, &mut reporter);
        feed(json!({"type": "RUN_STARTED", "threadId": "t", "runId": "r1"}));
        for i in 0..100_000 {
            feed(json!({"type": "THINKING_START", "messageId": format!("m{i}")}));
        }
        feed(json!({"type": "RUN_STARTED", "threadId": "t", "runId": "r2"}));
        feed(json!({"type": "THINKING_START", "messageId": "only"}));
        feed(json!({"type": "RUN_STARTED", "threadId": "t", "runId": "r3"}));
        let cap = boundary.thinking.by_id.capacity();
        assert!(
            cap <= crate::pipeline::RETAIN_FLOOR,
            "by_id capacity {cap} retained after small runs"
        );
    }
}
