//! Producer-side helpers for rules the agent author owns but the SDK can
//! check (sdk-assistable rows).
//!
//! * [`PartFilter`]: skip content parts the model cannot take, without
//!   failing the run, with a warning, and answer a tool call with `""` when
//!   every part was dropped (RUNIN-11, RUNIN-15, RUNIN-16, TOOL-17, TOOL-18,
//!   TOOL-23).
//! * [`context_prompt`]: render `context` entries for the model (RUNIN-22).
//! * [`flatten_for_legacy_peer`]: string-only results for a peer that sent no
//!   `protocolVersion`, warning about every dropped part (TOOL-21, TOOL-22).
//! * [`InterruptStore`]: an opt-in record of raised interrupts, to recognise
//!   unknown resume entries and refuse to act on missing ones
//!   (INTR-18..INTR-23). [`MemoryInterruptStore`] is the bounded in-process
//!   implementation; implement the trait for a shared database.

use std::collections::{BTreeMap, HashMap};
use std::future::Future;
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

use crate::content::{Content, ContentPart};
use crate::diagnostics::{Diagnostic, DiagnosticCode, Reporter, Scope};
use crate::error::{Error, ErrorCode, Result};
use crate::ids::{InterruptId, ThreadId};
use crate::interrupt::{Interrupt, ResumeEntry};
use crate::message::Message;
use crate::run_input::Context;

/// Filters content parts by what the model accepts.
pub struct PartFilter {
    accepts: Box<dyn Fn(&ContentPart) -> bool + Send + Sync>,
}

impl PartFilter {
    /// A filter keeping parts for which `accepts` is true.
    pub fn new(accepts: impl Fn(&ContentPart) -> bool + Send + Sync + 'static) -> Self {
        Self {
            accepts: Box::new(accepts),
        }
    }

    /// A filter keeping only text parts.
    #[must_use]
    pub fn text_only() -> Self {
        Self::new(|part| matches!(part, ContentPart::Text(_)))
    }

    /// Filters a message's parts in place, warning for each skipped part.
    /// Only user and tool messages carry parts; others are left alone. A
    /// message whose every part was skipped is left with `""`.
    ///
    /// Diagnostic pointers are relative to the message (`/content/N`), not to
    /// the run input, so parts skipped in different messages share a
    /// pointer. To filter run input, use [`filter_messages`], whose pointers
    /// are rooted in it.
    ///
    /// [`filter_messages`]: PartFilter::filter_messages
    pub fn filter_message(&self, message: &mut Message, reporter: &mut Reporter) {
        self.filter_in_place(message, None, reporter);
    }

    /// Filters every message in place by the rule of [`filter_message`],
    /// warning for each skipped part with a pointer
    /// `/messages/M/content/N` rooted in the run input.
    ///
    /// `M` is the message's position in `messages`. Pass the input's whole
    /// message list, in order (for example `ctx.messages().to_vec()`): if
    /// messages were dropped or reordered first, the pointers no longer
    /// name positions in the run input.
    ///
    /// [`filter_message`]: PartFilter::filter_message
    pub fn filter_messages(&self, messages: &mut [Message], reporter: &mut Reporter) {
        for (position, message) in messages.iter_mut().enumerate() {
            self.filter_in_place(message, Some(position), reporter);
        }
    }

    fn filter_in_place(
        &self,
        message: &mut Message,
        position: Option<usize>,
        reporter: &mut Reporter,
    ) {
        let content = match message {
            Message::User(m) => &mut m.content,
            Message::Tool(m) => &mut m.content,
            _ => return,
        };
        if let Content::Parts(_) = content {
            let taken = std::mem::replace(content, Content::Text(String::new()));
            *content = self.filter(taken, reporter, Scope::RunInput, position);
        }
    }

    /// Filters tool-result content. All parts dropped yields `""`, never a
    /// placeholder (TOOL-18, TOOL-23).
    pub fn filter_result(&self, content: Content, reporter: &mut Reporter) -> Content {
        self.filter(content, reporter, Scope::Event, None)
    }

    /// `message` is the position of the filtered message in the run input,
    /// when the pointer is to be rooted there.
    fn filter(
        &self,
        content: Content,
        reporter: &mut Reporter,
        scope: Scope,
        message: Option<usize>,
    ) -> Content {
        let parts = match content {
            Content::Parts(parts) => parts,
            other => return other,
        };
        let mut kept = Vec::with_capacity(parts.len());
        for (index, part) in parts.into_iter().enumerate() {
            if (self.accepts)(&part) {
                kept.push(part);
            } else {
                // Skip and say so; never fail the run over it (RUNIN-11,
                // RUNIN-15, RUNIN-16, TOOL-17).
                let kind = part_kind(&part);
                reporter.report(|| {
                    Diagnostic::new(
                        DiagnosticCode::ContentPartSkipped,
                        scope,
                        match message {
                            Some(m) => format!("/messages/{m}/content/{index}"),
                            None => format!("/content/{index}"),
                        },
                        format!("{kind} part skipped: the model cannot take it"),
                    )
                });
            }
        }
        if kept.is_empty() {
            Content::Text(String::new())
        } else {
            Content::Parts(kept)
        }
    }
}

impl std::fmt::Debug for PartFilter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PartFilter").finish_non_exhaustive()
    }
}

fn part_kind(part: &ContentPart) -> &'static str {
    match part {
        ContentPart::Text(_) => "text",
        ContentPart::Image(_) => "image",
        ContentPart::Audio(_) => "audio",
        ContentPart::Video(_) => "video",
        ContentPart::Document(_) => "document",
    }
}

/// Renders `context` entries as one grounding block, one entry per paragraph:
/// `description: value`, every entry included and in order (RUNIN-22).
#[must_use]
pub fn context_prompt(context: &[Context]) -> String {
    let size = context
        .iter()
        .map(|c| c.description.len() + c.value.len() + 4)
        .sum();
    let mut out = String::with_capacity(size);
    for (index, entry) in context.iter().enumerate() {
        if index > 0 {
            out.push_str("\n\n");
        }
        out.push_str(&entry.description);
        out.push_str(": ");
        out.push_str(&entry.value);
    }
    out
}

/// Flattens content to text for a peer predating content parts, warning
/// for each dropped part (TOOL-22). Text parts are concatenated in order;
/// nothing is put where a dropped part was (TOOL-23).
pub fn flatten_for_legacy_peer(content: Content, reporter: &mut Reporter) -> String {
    let parts = match content {
        Content::Text(text) => return text,
        Content::Parts(parts) => parts,
    };
    let mut out = String::new();
    for (index, part) in parts.iter().enumerate() {
        match part {
            ContentPart::Text(text) => out.push_str(&text.text),
            other => {
                let kind = part_kind(other);
                reporter.report(|| {
                    Diagnostic::new(
                        DiagnosticCode::LossyDowngrade,
                        Scope::Event,
                        format!("/content/{index}"),
                        format!(
                            "{kind} part dropped: the client predates content parts (0.0.40); \
                             upgrade the client to AG-UI 1.0 to receive it"
                        ),
                    )
                });
            }
        }
    }
    out
}

/// What an [`InterruptStore`] found in a resuming input.
///
/// A store built on a database loads the thread's recorded interrupts and
/// calls [`ResumeCheck::classify`], which applies INTR-18..INTR-23 the same
/// way [`MemoryInterruptStore`] does; [`ResumeCheck::new`] builds one from
/// lists already sorted.
#[derive(Debug, Clone, Default, PartialEq)]
#[non_exhaustive]
pub struct ResumeCheck {
    /// Entries answering interrupts this thread raised.
    pub recognised: Vec<ResumeEntry>,
    /// Entries naming interrupts the thread did not raise (or that expired
    /// from the store): proceed without them and warn (INTR-18, INTR-19).
    pub unrecognised: Vec<ResumeEntry>,
    /// Interrupts with no entry. Never act on these and never finish the run
    /// as success while they stand (INTR-20, INTR-21, INTR-23).
    pub uncovered: Vec<Interrupt>,
}

/// A record of the interrupts each thread's last run raised. Opt-in: a
/// producer need not keep inter-run state to be conformant (INTR-13).
///
/// Async, so a store can live in a database shared by several servers.
/// Methods added later will have default bodies.
pub trait InterruptStore: Send + Sync + 'static {
    /// Records the interrupts a run ended with, replacing the thread's
    /// previous record.
    fn record(
        &self,
        thread: &ThreadId,
        interrupts: Vec<Interrupt>,
    ) -> impl Future<Output = Result<()>> + Send;

    /// Sorts a resuming input's entries against the record. The caller
    /// warns about [`ResumeCheck::unrecognised`].
    fn check(
        &self,
        thread: &ThreadId,
        entries: &[ResumeEntry],
    ) -> impl Future<Output = Result<ResumeCheck>> + Send;

    /// Forgets a resolved interrupt.
    fn resolve(
        &self,
        thread: &ThreadId,
        interrupt: &InterruptId,
    ) -> impl Future<Output = Result<()>> + Send;
}

/// In-memory [`InterruptStore`], bounded: at most `max_threads` threads,
/// whose ids total at most 1 KiB per allowed thread (least recently
/// recorded evicted first), and an optional time to live. Thread ids are
/// chosen by the client, so the byte bound keeps a client from pinning
/// multi-megabyte ids; one id over the whole budget is still kept, alone.
/// Interrupt payloads are the agent's and are not counted. An evicted
/// thread's entries come back as unrecognised, never as covered.
#[derive(Debug)]
pub struct MemoryInterruptStore {
    max_threads: usize,
    ttl: Option<Duration>,
    open: Mutex<Records>,
}

/// The store's contents: records by thread plus their recording order, so
/// eviction of the least recently recorded thread is O(log n).
#[derive(Debug, Default)]
struct Records {
    by_thread: HashMap<ThreadId, Record>,
    order: BTreeMap<u64, ThreadId>,
    next: u64,
    /// Total bytes of the thread ids held (docs/DECISIONS.md D27).
    id_bytes: usize,
}

/// Thread-id bytes the store holds per allowed thread (D27).
const THREAD_ID_BUDGET: usize = 1024;

#[derive(Debug)]
struct Record {
    interrupts: Vec<Interrupt>,
    recorded: Instant,
    seq: u64,
}

impl Records {
    fn remove(&mut self, thread: &ThreadId) {
        if let Some(record) = self.by_thread.remove(thread) {
            self.order.remove(&record.seq);
            self.id_bytes = self.id_bytes.saturating_sub(thread.as_str().len());
        }
    }

    /// The thread's open interrupts, unless expired (an expired record is
    /// dropped and reads as nothing raised).
    fn live(&mut self, thread: &ThreadId, ttl: Option<Duration>) -> &[Interrupt] {
        let expired = match (self.by_thread.get(thread), ttl) {
            (Some(record), Some(ttl)) => record.recorded.elapsed() > ttl,
            _ => false,
        };
        if expired {
            self.remove(thread);
        }
        self.by_thread
            .get(thread)
            .map_or(&[], |record| record.interrupts.as_slice())
    }
}

impl MemoryInterruptStore {
    /// A store holding at most `max_threads` threads, with thread ids
    /// totalling at most 1 KiB per allowed thread (least recently recorded
    /// evicted first), entries expiring after `ttl` when set. A host whose
    /// thread ids are longer than 1 KiB on average sees the store hold
    /// fewer threads.
    #[must_use]
    pub fn new(max_threads: usize, ttl: Option<Duration>) -> Self {
        Self {
            max_threads: max_threads.max(1),
            ttl,
            open: Mutex::new(Records::default()),
        }
    }

    fn records(&self) -> Result<MutexGuard<'_, Records>> {
        self.open.lock().map_err(|_| {
            Error::new(
                ErrorCode::Internal,
                "the interrupt store is unavailable after a panic",
            )
        })
    }
}

impl Default for MemoryInterruptStore {
    /// 10,000 threads (thread ids totalling at most 10,000 KiB), 24 h time
    /// to live.
    fn default() -> Self {
        Self::new(10_000, Some(Duration::from_secs(24 * 60 * 60)))
    }
}

impl InterruptStore for MemoryInterruptStore {
    async fn record(&self, thread: &ThreadId, interrupts: Vec<Interrupt>) -> Result<()> {
        let mut records = self.records()?;
        records.remove(thread);
        if interrupts.is_empty() {
            return Ok(());
        }
        let budget = self.max_threads.saturating_mul(THREAD_ID_BUDGET);
        let len = thread.as_str().len();
        while !records.by_thread.is_empty()
            && (records.by_thread.len() >= self.max_threads
                || records.id_bytes.saturating_add(len) > budget)
        {
            let oldest = match records.order.iter().next() {
                Some((_, thread)) => thread.clone(),
                None => break,
            };
            records.remove(&oldest);
        }
        let seq = records.next;
        records.next += 1;
        records.order.insert(seq, thread.clone());
        records.id_bytes = records.id_bytes.saturating_add(len);
        records.by_thread.insert(
            thread.clone(),
            Record {
                interrupts,
                recorded: Instant::now(),
                seq,
            },
        );
        Ok(())
    }

    async fn check(&self, thread: &ThreadId, entries: &[ResumeEntry]) -> Result<ResumeCheck> {
        let mut records = self.records()?;
        let open = records.live(thread, self.ttl);
        let check = ResumeCheck::classify(open, entries);
        Ok(check)
    }

    async fn resolve(&self, thread: &ThreadId, interrupt: &InterruptId) -> Result<()> {
        let mut records = self.records()?;
        let emptied = match records.by_thread.get_mut(thread) {
            Some(record) => {
                record.interrupts.retain(|i| &i.id != interrupt);
                record.interrupts.is_empty()
            }
            None => false,
        };
        if emptied {
            records.remove(thread);
        }
        Ok(())
    }
}

impl ResumeCheck {
    /// A check from lists already sorted: entries answering raised
    /// interrupts, entries answering nothing raised, and raised interrupts
    /// with no entry.
    #[must_use]
    pub fn new(
        recognised: Vec<ResumeEntry>,
        unrecognised: Vec<ResumeEntry>,
        uncovered: Vec<Interrupt>,
    ) -> Self {
        Self {
            recognised,
            unrecognised,
            uncovered,
        }
    }

    /// Sorts a resuming input's `entries` against the interrupts the thread
    /// has open (`open`, as recorded by [`InterruptStore::record`]): an entry
    /// naming an open interrupt is recognised, any other entry is
    /// unrecognised (INTR-18), and an open interrupt no entry names is
    /// uncovered (INTR-20, INTR-21, INTR-23). Input order is kept.
    ///
    /// ```
    /// use agui_sdk::interrupt::{Interrupt, ResumeEntry, ResumeStatus};
    /// use agui_sdk::server::helpers::ResumeCheck;
    ///
    /// let open = [Interrupt::new("i-1", "approval"), Interrupt::new("i-2", "approval")];
    /// let entries = [ResumeEntry::new("i-1", ResumeStatus::Resolved), ResumeEntry::new("i-9", ResumeStatus::Resolved)];
    /// let check = ResumeCheck::classify(&open, &entries);
    /// assert_eq!(check.recognised.len(), 1);
    /// assert_eq!(check.unrecognised[0].interrupt_id.as_str(), "i-9");
    /// assert_eq!(check.uncovered[0].id.as_str(), "i-2");
    /// ```
    #[must_use]
    pub fn classify(open: &[Interrupt], entries: &[ResumeEntry]) -> Self {
        let mut check = Self {
            recognised: Vec::with_capacity(entries.len()),
            unrecognised: Vec::new(),
            uncovered: Vec::new(),
        };
        for entry in entries {
            if open.iter().any(|i| i.id == entry.interrupt_id) {
                check.recognised.push(entry.clone());
            } else {
                // An answer to nothing this thread asked (INTR-18).
                check.unrecognised.push(entry.clone());
            }
        }
        check.uncovered = open
            .iter()
            .filter(|i| !entries.iter().any(|e| e.interrupt_id == i.id))
            .cloned()
            .collect();
        check
    }

    /// Reports each [`unrecognised`](Self::unrecognised) entry as a
    /// `resume.unknown_entry` warning, pointing into the input's `resume`
    /// list, so the run can proceed without them (INTR-18, INTR-19).
    pub fn report_unrecognised(&self, entries: &[ResumeEntry], reporter: &mut Reporter) {
        // One forward pass over the (peer-supplied) list against a multiset of
        // the unrecognised ids: linear, and each duplicate gets its own index.
        let mut pending: HashMap<&InterruptId, usize> =
            HashMap::with_capacity(self.unrecognised.len());
        for unknown in &self.unrecognised {
            *pending.entry(&unknown.interrupt_id).or_insert(0) += 1;
        }
        let report = |reporter: &mut Reporter, pointer: String| {
            reporter.report(|| {
                Diagnostic::new(
                    DiagnosticCode::UnknownResumeEntry,
                    Scope::RunInput,
                    pointer,
                    "resume entry names an interrupt this thread did not raise; ignored",
                )
            });
        };
        for (index, entry) in entries.iter().enumerate() {
            if pending.is_empty() {
                break;
            }
            if let Some(left) = pending.get_mut(&entry.interrupt_id) {
                *left -= 1;
                if *left == 0 {
                    pending.remove(&entry.interrupt_id);
                }
                report(reporter, format!("/resume/{index}"));
            }
        }
        // Entries the check lists but `entries` lacks (possible when the check
        // was built with `ResumeCheck::new` or edited, or a different list is
        // passed): point at the list, never an invented index.
        for left in pending.into_values() {
            for _ in 0..left {
                report(reporter, "/resume".to_owned());
            }
        }
    }
}
