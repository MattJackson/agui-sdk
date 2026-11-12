//! In-place, all-or-nothing application with an undo log.
//!
//! Each operation changes the document in place and records at most one undo
//! entry holding exactly what it displaced (moved into the log, never
//! cloned). On failure the entries are replayed newest first; each replay
//! sees the document exactly as it was right after the matching forward
//! step, so every parent pointer still resolves, even after `move`s shifted
//! array indices. On success the log is dropped. Entries borrow their pointer
//! text from the operations, so logging allocates nothing per operation
//! except a boxed entry per `move`, and the first entry is stored inline: a
//! one-operation patch other than a `move` allocates no log.

use std::mem;

use serde_json::{Map, Value};

use super::pointer::{self, Index, RawTokens};
use super::walk::{self, Stop};
use super::{PatchErrorReason as Reason, PatchOp, PatchOperation};

/// The last step of a location: how to reach the slot from its parent.
#[derive(Debug, Clone, Copy)]
enum Last<'p> {
    Root,
    Index(usize),
    /// The escaped token naming an object member, and the member's position
    /// in its object (used to restore order under `preserve-order`).
    Key {
        raw: &'p str,
        pos: usize,
    },
}

#[derive(Debug, Clone, Copy)]
struct Loc<'p> {
    parent: &'p str,
    last: Last<'p>,
}

const ROOT: Loc<'static> = Loc {
    parent: "",
    last: Last::Root,
};

#[derive(Debug)]
enum Undo<'p> {
    /// A value was inserted (array element or new member): take it out.
    Inserted(Loc<'p>),
    /// A value was overwritten: put the old one back.
    Replaced(Loc<'p>, Value),
    /// A value was removed: re-insert it, reusing the original key string.
    Removed(Loc<'p>, Option<String>, Value),
    /// A value moved from `from` to `to`, possibly displacing one there.
    /// Boxed: the rare `move` would otherwise size every entry of the log
    /// (152 bytes instead of 104).
    Moved(Box<Moved<'p>>),
}

#[derive(Debug)]
struct Moved<'p> {
    from: Loc<'p>,
    key: Option<String>,
    to: Loc<'p>,
    displaced: Option<Value>,
}

/// Undo entries reserved up front, at most.
const LOG_RESERVE: usize = 4096;

#[derive(Debug, Default)]
struct Log<'p> {
    first: Option<Undo<'p>>,
    rest: Vec<Undo<'p>>,
}

impl<'p> Log<'p> {
    fn push(&mut self, entry: Undo<'p>) {
        if self.first.is_none() {
            self.first = Some(entry);
        } else {
            self.rest.push(entry);
        }
    }

    fn len(&self) -> usize {
        usize::from(self.first.is_some()) + self.rest.len()
    }

    /// The value entry `i` displaced, if any.
    fn displaced_at(&self, i: usize) -> Option<&Value> {
        let entry = if i == 0 {
            self.first.as_ref()
        } else {
            self.rest.get(i - 1)
        }?;
        match entry {
            Undo::Inserted(_) => None,
            Undo::Replaced(_, v) | Undo::Removed(_, _, v) => Some(v),
            Undo::Moved(m) => m.displaced.as_ref(),
        }
    }

    fn newest_first(self) -> impl Iterator<Item = Undo<'p>> {
        self.rest.into_iter().rev().chain(self.first)
    }
}

fn step_ref<'v>(v: &'v Value, raw: &str) -> Result<&'v Value, Reason> {
    match v {
        Value::Object(m) => m.get(&*pointer::unescape(raw)).ok_or(Reason::PathNotFound),
        Value::Array(a) => match pointer::parse_index(raw) {
            Some(Index::At(i)) => a.get(i).ok_or(Reason::InvalidIndex),
            _ => Err(Reason::InvalidIndex),
        },
        _ => Err(Reason::PathNotFound),
    }
}

fn step_mut<'v>(v: &'v mut Value, raw: &str) -> Result<&'v mut Value, Reason> {
    match v {
        Value::Object(m) => m
            .get_mut(&*pointer::unescape(raw))
            .ok_or(Reason::PathNotFound),
        Value::Array(a) => match pointer::parse_index(raw) {
            Some(Index::At(i)) => a.get_mut(i).ok_or(Reason::InvalidIndex),
            _ => Err(Reason::InvalidIndex),
        },
        _ => Err(Reason::PathNotFound),
    }
}

fn resolve_ref<'v>(doc: &'v Value, ptr: &str) -> Result<&'v Value, Reason> {
    RawTokens::new(ptr).try_fold(doc, step_ref)
}

fn resolve_mut<'v>(doc: &'v mut Value, ptr: &str) -> Result<&'v mut Value, Reason> {
    let mut cur = doc;
    for raw in RawTokens::new(ptr) {
        cur = step_mut(cur, raw)?;
    }
    Ok(cur)
}

// ----- undo primitives: never fail when replayed newest first -----

fn slot_at<'v>(doc: &'v mut Value, loc: Loc<'_>) -> Option<&'v mut Value> {
    match loc.last {
        Last::Root => Some(doc),
        Last::Index(i) => match resolve_mut(doc, loc.parent).ok()? {
            Value::Array(a) => a.get_mut(i),
            _ => None,
        },
        Last::Key { raw, .. } => match resolve_mut(doc, loc.parent).ok()? {
            Value::Object(m) => m.get_mut(&*pointer::unescape(raw)),
            _ => None,
        },
    }
}

fn take_at(doc: &mut Value, loc: Loc<'_>) -> Option<Value> {
    match loc.last {
        Last::Root => Some(mem::take(doc)),
        Last::Index(i) => match resolve_mut(doc, loc.parent).ok()? {
            Value::Array(a) if i < a.len() => Some(a.remove(i)),
            _ => None,
        },
        Last::Key { raw, .. } => match resolve_mut(doc, loc.parent).ok()? {
            Value::Object(m) => map_remove_last(m, &pointer::unescape(raw)),
            _ => None,
        },
    }
}

fn put_at(doc: &mut Value, loc: Loc<'_>, key: Option<String>, v: Value) -> Option<()> {
    match loc.last {
        Last::Root => {
            *doc = v;
            Some(())
        }
        Last::Index(i) => match resolve_mut(doc, loc.parent).ok()? {
            Value::Array(a) if i <= a.len() => {
                a.insert(i, v);
                Some(())
            }
            _ => None,
        },
        Last::Key { raw, pos } => match resolve_mut(doc, loc.parent).ok()? {
            Value::Object(m) => {
                let key = key.unwrap_or_else(|| pointer::unescape(raw).into_owned());
                map_restore(m, pos, key, v);
                Some(())
            }
            _ => None,
        },
    }
}

fn rollback(doc: &mut Value, log: Log<'_>) {
    for entry in log.newest_first() {
        let replayed = match entry {
            Undo::Inserted(loc) => take_at(doc, loc).map(drop),
            Undo::Replaced(loc, old) => slot_at(doc, loc).map(|slot| *slot = old),
            Undo::Removed(loc, key, v) => put_at(doc, loc, key, v),
            Undo::Moved(moved) => {
                let Moved {
                    from,
                    key,
                    to,
                    displaced,
                } = *moved;
                let moved = match displaced {
                    Some(old) => slot_at(doc, to).map(|slot| mem::replace(slot, old)),
                    None => take_at(doc, to),
                };
                moved.and_then(|v| put_at(doc, from, key, v))
            }
        };
        debug_assert!(replayed.is_some(), "undo entry failed to replay");
    }
}

/// Array elements moved per walk unit: 16 `Value`s is 512 bytes of memmove
/// with the default 32-byte `Value` (1,152 bytes under `preserve-order`,
/// where a `Value` is 72 bytes), roughly the cost of one node visit. A
/// patch's total array shifting is therefore at most `16 x walk_cap`
/// element moves (2 GB of memmove at the default 4,000,000-unit walk
/// budget, about 4.6 GB under `preserve-order`; proportionally more for a
/// document over 250,000 units), linear in the node budget.
const SHIFT_PER_UNIT: usize = 16;

/// Object members shifted per walk unit under `preserve-order`: each is a
/// 104-byte `IndexMap` entry (hash, key, `Value`) whose hash-table index is
/// rewritten as it moves, so 4 entries (416 bytes plus 4 index updates) per
/// unit, and a patch shifts at most `4 x walk_cap` members (about 1.7 GB of
/// memmove at the default walk budget).
#[cfg(feature = "preserve-order")]
const MEMBER_SHIFT_PER_UNIT: usize = 4;

/// Walk cost of moving `moved` array elements. Fewer than 16 are free.
fn shift_cost(moved: usize) -> usize {
    moved / SHIFT_PER_UNIT
}

/// Whether a successful `op` pushes an undo entry: every operation does,
/// except `test` and a `move` onto itself.
fn logs_undo(op: &PatchOperation) -> bool {
    match op.op {
        PatchOp::Test => false,
        PatchOp::Move => op.from.as_ref().map(|f| f.as_str()) != Some(op.path.as_str()),
        _ => true,
    }
}

/// Walk cost of removing one member from an object of `len` members:
/// O(members) with `preserve-order` (position scan plus shift), O(log n)
/// otherwise.
#[cfg(feature = "preserve-order")]
pub(super) fn remove_shift(len: usize) -> usize {
    len / MEMBER_SHIFT_PER_UNIT
}

#[cfg(not(feature = "preserve-order"))]
pub(super) fn remove_shift(_len: usize) -> usize {
    0
}

/// Charges `n` units against a walk budget held in disjoint fields, so it can
/// be used while the document is borrowed.
fn charge_to(walked: &mut usize, cap: usize, n: usize) -> Result<(), Reason> {
    match walked.checked_add(n) {
        Some(w) if w <= cap => {
            *walked = w;
            Ok(())
        }
        _ => Err(Reason::BudgetExceeded),
    }
}

/// Removes the member `place` appended last (undoing an insert). Under
/// `preserve-order` that is O(1); a general removal would scan every key.
#[cfg(feature = "preserve-order")]
fn map_remove_last(m: &mut Map<String, Value>, key: &str) -> Option<Value> {
    debug_assert!(m.keys().next_back().is_some_and(|k| k == key));
    m.shift_remove(key)
}

#[cfg(not(feature = "preserve-order"))]
fn map_remove_last(m: &mut Map<String, Value>, key: &str) -> Option<Value> {
    m.remove(key)
}

/// Removes a member, returning its position, key and value. Under
/// `preserve-order` later members shift up (never a swap), so order survives;
/// that costs O(members). The position is found by comparing addresses with
/// the key the map itself returns, never key bytes, so the scan is O(members)
/// whatever the key length.
#[cfg(feature = "preserve-order")]
fn map_remove(m: &mut Map<String, Value>, key: &str) -> Option<(usize, String, Value)> {
    let (found, _) = m.get_key_value(key)?;
    let pos = m.keys().position(|k| std::ptr::eq(k, found))?;
    let (k, v) = m.shift_remove_entry(key)?;
    Some((pos, k, v))
}

#[cfg(not(feature = "preserve-order"))]
fn map_remove(m: &mut Map<String, Value>, key: &str) -> Option<(usize, String, Value)> {
    let (k, v) = m.remove_entry(key)?;
    Some((0, k, v))
}

/// Re-inserts a member at its original position (sorted maps place it by key).
#[cfg(feature = "preserve-order")]
fn map_restore(m: &mut Map<String, Value>, pos: usize, key: String, v: Value) {
    let pos = pos.min(m.len());
    m.shift_insert(pos, key, v);
}

#[cfg(not(feature = "preserve-order"))]
fn map_restore(m: &mut Map<String, Value>, _pos: usize, key: String, v: Value) {
    m.insert(key, v);
}

/// Result of a run: the node count after success, or the failing operation.
pub(super) enum Outcome {
    Applied {
        nodes: usize,
        /// Estimated resident bytes the patch placed beyond the values it
        /// carries: the values `copy` duplicated, and the slot every `add`,
        /// `copy` and `move` placed a value in (0 unless measured).
        copied: usize,
        /// The slots `remove` and `move` sources vacated, as (operation
        /// index, bytes): what [`crate::json::slot_bytes`] charged for each
        /// at the length it was vacated at (empty unless measured).
        vacated: Vec<(usize, usize)>,
    },
    Failed {
        index: usize,
        reason: Reason,
    },
    /// Every operation applied but the caller's check refused the result; the
    /// document has been restored.
    Refused,
}

/// What one patch may spend.
pub(super) struct Caps {
    /// Node budget (never above `ceiling`).
    pub(super) nodes: usize,
    /// Absolute node ceiling.
    pub(super) ceiling: usize,
    /// Walk budget.
    pub(super) walk: usize,
    /// Pointer depth.
    pub(super) depth: usize,
    /// Measure the resident size of every value `copy` duplicates and of
    /// every slot a value is placed in.
    pub(super) measure_copies: bool,
}

pub(super) struct Engine<'p, 'd> {
    doc: &'d mut Value,
    log: Log<'p>,
    /// Nodes in the document, or more: displaced values are credited lazily
    /// (see `settle`), so this is an upper bound until `settled == log.len()`.
    nodes: usize,
    /// Log entries whose displaced values are already credited to `nodes`.
    settled: usize,
    /// Nodes of entry `settled`'s displaced value already credited (a lower
    /// bound on its size, from a count abandoned at its bound).
    partial: usize,
    node_cap: usize,
    ceiling: usize,
    walked: usize,
    walk_cap: usize,
    max_depth: usize,
    measure_copies: bool,
    /// Resident bytes of the copies and slots placed so far (see
    /// `measure_copies`).
    copied: usize,
    /// Slots vacated so far (see `measure_copies` and [`Outcome::Applied`]).
    vacated: Vec<(usize, usize)>,
    /// Index of the operation running.
    op: usize,
}

impl<'p, 'd> Engine<'p, 'd> {
    pub(super) fn new(doc: &'d mut Value, nodes: usize, caps: Caps) -> Self {
        Engine {
            doc,
            log: Log::default(),
            nodes,
            settled: 0,
            partial: 0,
            // A document already above a limit (a text-heavy snapshot, or a
            // ceiling lowered by other documents' growth) may still take a
            // patch that does not grow it: only growth past where the patch
            // started is refused (docs/DECISIONS.md D14).
            node_cap: caps.nodes.max(nodes),
            ceiling: caps.ceiling.max(nodes),
            walked: 0,
            walk_cap: caps.walk,
            max_depth: caps.depth,
            measure_copies: caps.measure_copies,
            copied: 0,
            vacated: Vec::new(),
            op: 0,
        }
    }

    /// Applies every operation, or none.
    pub(super) fn run(
        mut self,
        ops: &'p [PatchOperation],
        accept: impl FnOnce(&Value) -> bool,
    ) -> Outcome {
        // Each logging operation pushes one entry and the first is inline:
        // one allocation for the rest instead of doubling growth. Capped, so
        // a long patch that fails early reserves at most LOG_RESERVE entries;
        // `test`s, which log nothing, reserve nothing.
        let logging = ops.iter().filter(|o| logs_undo(o)).count();
        self.log
            .rest
            .reserve_exact(logging.saturating_sub(1).min(LOG_RESERVE));
        for (index, op) in ops.iter().enumerate() {
            self.op = index;
            if let Err(reason) = self.run_op(op) {
                rollback(self.doc, mem::take(&mut self.log));
                return Outcome::Failed { index, reason };
            }
        }
        // A refusal never pays for counting what the patch displaced.
        if !accept(self.doc) {
            rollback(self.doc, mem::take(&mut self.log));
            return Outcome::Refused;
        }
        debug_assert_eq!(
            ops.iter().filter(|o| logs_undo(o)).count(),
            self.log.len(),
            "every logging operation pushes exactly one undo entry"
        );
        if let Err(reason) = self.settle() {
            // The operation that displaced the subtree that could not be
            // counted is the one whose log entry was being credited.
            let index = ops
                .iter()
                .enumerate()
                .filter(|(_, o)| logs_undo(o))
                .nth(self.settled)
                .map_or(ops.len().saturating_sub(1), |(i, _)| i);
            rollback(self.doc, mem::take(&mut self.log));
            return Outcome::Failed { index, reason };
        }
        Outcome::Applied {
            nodes: self.nodes,
            copied: self.copied,
            vacated: self.vacated,
        }
    }

    fn charge(&mut self, n: usize) -> Result<(), Reason> {
        charge_to(&mut self.walked, self.walk_cap, n)
    }

    fn walk_left(&self) -> usize {
        self.walk_cap.saturating_sub(self.walked)
    }

    /// The reason for exceeding the node budget: the absolute ceiling when
    /// that is what binds.
    fn over_nodes(&self) -> Reason {
        if self.node_cap >= self.ceiling {
            Reason::NodeCeilingExceeded
        } else {
            Reason::BudgetExceeded
        }
    }

    /// Adds `n` nodes. Over a limit, credits the displaced values not yet
    /// counted before refusing: counting them is deferred until the budget
    /// would otherwise bind, so an operation that fails for another reason
    /// never pays for a subtree it displaced.
    fn add_nodes(&mut self, n: usize) -> Result<(), Reason> {
        self.nodes = self.nodes.saturating_add(n);
        let limit = self.node_cap.min(self.ceiling);
        if self.nodes > limit {
            self.credit(self.nodes - limit)?;
        }
        if self.nodes > self.ceiling {
            Err(Reason::NodeCeilingExceeded)
        } else if self.nodes > self.node_cap {
            Err(self.over_nodes())
        } else {
            Ok(())
        }
    }

    /// Credits every displaced value not yet counted (commit).
    fn settle(&mut self) -> Result<(), Reason> {
        self.credit(usize::MAX)
    }

    /// Credits at least `deficit` nodes of displaced values, oldest first,
    /// counting no further than that: a count abandoned at its bound credits
    /// the bound (a lower bound on the value) and is resumed, at double the
    /// bound, only if a later check needs more. A frame that fails for
    /// another reason therefore pays for the deficit, not the subtree. The
    /// count is charged to the walk budget.
    fn credit(&mut self, mut deficit: usize) -> Result<(), Reason> {
        while deficit > 0 && self.settled < self.log.len() {
            let Some(v) = self.log.displaced_at(self.settled) else {
                self.settled += 1;
                self.partial = 0;
                continue;
            };
            let partial = self.partial;
            let bound = partial
                .saturating_mul(2)
                .max(partial.saturating_add(deficit));
            let reach = partial.saturating_add(self.walk_left());
            let counted = walk::count_displaced(v, bound.min(reach));
            let (total, done) = match counted {
                Some(n) => (n, true),
                None if bound <= reach => (bound, false),
                None => return Err(Reason::BudgetExceeded),
            };
            let delta = total - partial;
            charge_to(&mut self.walked, self.walk_cap, delta)?;
            self.sub_nodes(delta);
            deficit = deficit.saturating_sub(delta);
            if !done {
                self.partial = total;
                break;
            }
            self.settled += 1;
            self.partial = 0;
        }
        Ok(())
    }

    fn sub_nodes(&mut self, n: usize) {
        self.nodes = self.nodes.saturating_sub(n);
    }

    /// Height left below a pointer of `depth` tokens.
    fn room(&self, depth: usize) -> Result<usize, Reason> {
        self.max_depth.checked_sub(depth).ok_or(Reason::TooDeep)
    }

    /// Validated pointer text of a required member.
    fn pointer_text(p: Option<&'p super::JsonPointer>) -> Result<&'p str, Reason> {
        let text = p.ok_or(Reason::MissingMember)?.as_str();
        if pointer::is_valid(text) {
            Ok(text)
        } else {
            Err(Reason::PathNotFound)
        }
    }

    /// Inserts `v` at `path` with `add` semantics. On failure the value is
    /// handed back so a `move` can put it where it came from. Also returns
    /// the node units of the key it inserted (zero unless a new object
    /// member), which the caller adds to the node count: a key is text
    /// retained by the document like any string.
    #[allow(clippy::type_complexity)]
    fn place(
        &mut self,
        path: &'p str,
        v: Value,
    ) -> Result<(Loc<'p>, Option<Value>, usize), (Reason, Value)> {
        let Some((parent, raw)) = pointer::split_last(path) else {
            return Ok((ROOT, Some(mem::replace(self.doc, v)), 0));
        };
        if let Err(r) = self.charge(pointer::depth(path)) {
            return Err((r, v));
        }
        let target = match resolve_mut(self.doc, parent) {
            Ok(t) => t,
            Err(r) => return Err((r, v)),
        };
        match target {
            Value::Object(m) => {
                let key = pointer::unescape(raw);
                if let Some(slot) = m.get_mut(&*key) {
                    let loc = Loc {
                        parent,
                        last: Last::Key { raw, pos: 0 },
                    };
                    return Ok((loc, Some(mem::replace(slot, v)), 0));
                }
                // A new member goes last; rollback takes it out by key.
                let loc = Loc {
                    parent,
                    last: Last::Key { raw, pos: m.len() },
                };
                let units = walk::text_units(key.len());
                m.insert(key.into_owned(), v);
                Ok((loc, None, units))
            }
            Value::Array(a) => {
                let i = match pointer::parse_index(raw) {
                    Some(Index::End) => a.len(),
                    Some(Index::At(i)) if i <= a.len() => i,
                    _ => return Err((Reason::InvalidIndex, v)),
                };
                if let Err(r) = charge_to(&mut self.walked, self.walk_cap, shift_cost(a.len() - i))
                {
                    return Err((r, v));
                }
                a.insert(i, v);
                Ok((
                    Loc {
                        parent,
                        last: Last::Index(i),
                    },
                    None,
                    0,
                ))
            }
            _ => Err((Reason::PathNotFound, v)),
        }
    }

    /// Removes the value at a non-root `path`. The root cannot be removed.
    fn take(&mut self, path: &'p str) -> Result<(Loc<'p>, Option<String>, Value), Reason> {
        let (parent, raw) = pointer::split_last(path).ok_or(Reason::PathNotFound)?;
        self.charge(pointer::depth(path))?;
        match resolve_mut(self.doc, parent)? {
            Value::Object(m) => {
                let name = pointer::unescape(raw);
                if !m.contains_key(&*name) {
                    return Err(Reason::PathNotFound);
                }
                charge_to(&mut self.walked, self.walk_cap, remove_shift(m.len()))?;
                let (pos, key, v) = map_remove(m, &name).ok_or(Reason::PathNotFound)?;
                if self.measure_copies {
                    // What placing `key` back would charge: the slot it left.
                    let freed = crate::json::member_slot_bytes(m.len(), key.len());
                    self.vacated.push((self.op, freed));
                }
                self.sub_nodes(walk::text_units(key.len()));
                let loc = Loc {
                    parent,
                    last: Last::Key { raw, pos },
                };
                Ok((loc, Some(key), v))
            }
            Value::Array(a) => match pointer::parse_index(raw) {
                Some(Index::At(i)) if i < a.len() => {
                    charge_to(&mut self.walked, self.walk_cap, shift_cost(a.len() - 1 - i))?;
                    let v = a.remove(i);
                    if self.measure_copies {
                        // What placing an element back would charge.
                        let freed = crate::json::element_slot_bytes(a.len());
                        self.vacated.push((self.op, freed));
                    }
                    let loc = Loc {
                        parent,
                        last: Last::Index(i),
                    };
                    Ok((loc, None, v))
                }
                _ => Err(Reason::InvalidIndex),
            },
            _ => Err(Reason::PathNotFound),
        }
    }

    /// Overwrites the existing value at `path`.
    fn overwrite(&mut self, path: &'p str, v: Value) -> Result<(Loc<'p>, Value), Reason> {
        let Some((parent, raw)) = pointer::split_last(path) else {
            return Ok((ROOT, mem::replace(self.doc, v)));
        };
        self.charge(pointer::depth(path))?;
        let parent_value = resolve_mut(self.doc, parent)?;
        let last = match parent_value {
            Value::Array(_) => match pointer::parse_index(raw) {
                Some(Index::At(i)) => Last::Index(i),
                _ => return Err(Reason::InvalidIndex),
            },
            _ => Last::Key { raw, pos: 0 },
        };
        let slot = step_mut(parent_value, raw)?;
        Ok((Loc { parent, last }, mem::replace(slot, v)))
    }

    /// The value an `add`/`replace` inserts: cloned from the operation,
    /// charged to the walk budget and bounded by the height left at the target.
    fn incoming(&mut self, op: &PatchOperation, room: usize) -> Result<(Value, usize), Reason> {
        let source = op.value.as_ref().ok_or(Reason::MissingMember)?;
        let (v, nodes) =
            walk::clone_bounded(source, self.walk_left(), room).map_err(|s| match s {
                Stop::Nodes => Reason::BudgetExceeded,
                Stop::Depth => Reason::TooDeep,
            })?;
        self.charge(nodes)?;
        Ok((v, nodes))
    }

    /// Logs what a placement displaced. Displaced values are credited later
    /// (`credit`), unless the caller already counted and charged `n` nodes of
    /// one (`counted = Some((n, exact))`, only when everything before it is
    /// settled): it is then settled, or partially credited if not `exact`.
    fn record_placed(
        &mut self,
        loc: Loc<'p>,
        displaced: Option<Value>,
        counted: Option<(usize, bool)>,
    ) {
        match displaced {
            Some(old) => {
                self.log.push(Undo::Replaced(loc, old));
                if let Some((n, exact)) = counted {
                    debug_assert_eq!(self.settled + 1, self.log.len());
                    self.sub_nodes(n);
                    if exact {
                        self.settled = self.log.len();
                        self.partial = 0;
                    } else {
                        self.partial = n;
                    }
                }
            }
            None => self.log.push(Undo::Inserted(loc)),
        }
    }

    fn run_op(&mut self, op: &'p PatchOperation) -> Result<(), Reason> {
        let path = Self::pointer_text(Some(&op.path))?;
        match op.op {
            PatchOp::Add => {
                let room = self.room(pointer::depth(path))?;
                let (v, nodes) = self.incoming(op, room)?;
                let slot = self.slot_bytes(path);
                let (loc, displaced, key) = self.place(path, v).map_err(|(r, _)| r)?;
                self.copied = self.copied.saturating_add(slot);
                self.record_placed(loc, displaced, None);
                self.add_nodes(nodes.saturating_add(key))
            }
            PatchOp::Remove => {
                let (loc, key, v) = self.take(path)?;
                self.log.push(Undo::Removed(loc, key, v));
                Ok(())
            }
            PatchOp::Replace => {
                let room = self.room(pointer::depth(path))?;
                let (v, nodes) = self.incoming(op, room)?;
                let (loc, old) = self.overwrite(path, v)?;
                self.log.push(Undo::Replaced(loc, old));
                self.add_nodes(nodes)
            }
            PatchOp::Move => self.run_move(Self::pointer_text(op.from.as_ref())?, path),
            PatchOp::Copy => self.run_copy(Self::pointer_text(op.from.as_ref())?, path),
            PatchOp::Test => {
                let expected = op.value.as_ref().ok_or(Reason::MissingMember)?;
                self.charge(pointer::depth(path))?;
                let mut budget = self.walk_left();
                let before = budget;
                let actual = resolve_ref(self.doc, path)?;
                let equal = walk::eq_bounded(actual, expected, &mut budget);
                self.walked += before - budget;
                match equal {
                    Some(true) => Ok(()),
                    Some(false) => Err(Reason::TestFailed),
                    None => Err(Reason::BudgetExceeded),
                }
            }
        }
    }

    fn run_move(&mut self, from: &'p str, path: &'p str) -> Result<(), Reason> {
        if from == path {
            // RFC 6902 §4.4: the location must exist; nothing moves.
            self.charge(pointer::depth(from))?;
            return resolve_ref(self.doc, from).map(drop);
        }
        if pointer::is_proper_prefix(from, path) {
            return Err(Reason::MoveIntoChild);
        }
        let (depth_from, depth_to) = (pointer::depth(from), pointer::depth(path));
        if depth_to > depth_from {
            // Moving deeper: the subtree must fit the height left at the
            // target. Moving up or level cannot break the depth invariant.
            let room = self.room(depth_to)?;
            self.charge(depth_from)?;
            let budget = self.walk_left();
            let source = resolve_ref(self.doc, from)?;
            let nodes = walk::measure(source, budget, room).map_err(|s| match s {
                Stop::Nodes => Reason::BudgetExceeded,
                Stop::Depth => Reason::TooDeep,
            })?;
            self.charge(nodes)?;
        }
        let (from_loc, key, v) = self.take(from)?;
        let old_key = key.as_ref().map_or(0, |k| walk::text_units(k.len()));
        let slot = self.slot_bytes(path);
        match self.place(path, v) {
            Ok((to, displaced, new_key)) => {
                self.copied = self.copied.saturating_add(slot);
                self.log.push(Undo::Moved(Box::new(Moved {
                    from: from_loc,
                    key,
                    to,
                    displaced,
                })));
                // `take` released the old key's units. A longer key is
                // growth like any other and must respect the ceiling.
                if new_key > old_key {
                    self.add_nodes(new_key)
                } else {
                    self.nodes = self.nodes.saturating_add(new_key);
                    Ok(())
                }
            }
            Err((reason, v)) => {
                self.log.push(Undo::Removed(from_loc, key, v));
                Err(reason)
            }
        }
    }

    fn run_copy(&mut self, from: &'p str, path: &'p str) -> Result<(), Reason> {
        let room = self.room(pointer::depth(path))?;
        // A missing source fails in O(depth), before anything is counted.
        self.charge(pointer::depth(from))?;
        resolve_ref(self.doc, from)?;
        // Optimistic first pass: displaced values are not yet credited, so
        // `self.nodes` is an upper bound and the target counts as zero. Only
        // if that refuses the copy are displaced values credited, and then
        // only as far as the copy needs: each retry asks for twice as much
        // (at least the headroom it had), so a frame that fails later pays
        // for its deficit, not for whole subtrees it displaced.
        let mut request = 0usize;
        // Nodes of an existing target member counted so far, and whether
        // that count is exact.
        let mut target = 0usize;
        let mut target_done = false;
        let mut displaced_hint = if path.is_empty() { self.nodes } else { 0 };
        let (v, nodes) = loop {
            let headroom = self
                .node_cap
                .saturating_add(displaced_hint)
                .saturating_sub(self.nodes);
            let walk_left = self.walk_left();
            let limit = headroom.min(walk_left);
            // Which budget a too-large copy breaks.
            let walk_binds = headroom > walk_left;
            let too_many = if walk_binds {
                Reason::BudgetExceeded
            } else {
                self.over_nodes()
            };
            // Copy bomb: the root's size is already known, so refuse in O(1).
            let root_bomb = from.is_empty() && self.nodes > limit;
            let refused = if root_bomb {
                Err(too_many)
            } else {
                let source = resolve_ref(self.doc, from)?;
                walk::clone_bounded(source, limit, room).map_err(|s| match s {
                    Stop::Nodes => too_many,
                    Stop::Depth => Reason::TooDeep,
                })
            };
            let r = match refused {
                Ok(done) => break done,
                Err(Reason::TooDeep) => return Err(Reason::TooDeep),
                // Crediting cannot make room when the walk budget binds (it
                // only shrinks) or the root is the target (its headroom is
                // the whole budget already); only the root bomb's upper
                // bound can still fall.
                Err(r) if (walk_binds || path.is_empty()) && !root_bomb => return Err(r),
                Err(r) => r,
            };
            request = request.saturating_mul(2).max(headroom).max(1);
            let before = (self.nodes, displaced_hint);
            self.credit(request)?;
            if path.is_empty() {
                displaced_hint = self.nodes;
            } else if !target_done && self.settled == self.log.len() {
                // Older displaced values are all credited, so the target is
                // next in line: count it, bounded like `credit`.
                if let Some(old) = self.target_member(path) {
                    let bound = target.saturating_mul(2).max(target.saturating_add(request));
                    let reach = target.saturating_add(self.walk_left());
                    let (total, done) = match walk::count_displaced(old, bound.min(reach)) {
                        Some(n) => (n, true),
                        None if bound <= reach => (bound, false),
                        None => return Err(Reason::BudgetExceeded),
                    };
                    self.charge(total - target)?;
                    target = total;
                    target_done = done;
                    displaced_hint = target;
                } else {
                    target_done = true;
                }
            }
            if (self.nodes, displaced_hint) == before {
                return Err(r);
            }
        };
        self.charge(nodes)?;
        let size = if self.measure_copies {
            crate::json::resident_bytes(&v).saturating_add(self.slot_bytes(path))
        } else {
            0
        };
        let (loc, displaced, key) = self.place(path, v).map_err(|(r, _)| r)?;
        self.copied = self.copied.saturating_add(size);
        let counted = (target > 0).then_some((target, target_done));
        self.record_placed(loc, displaced, counted);
        self.add_nodes(nodes.saturating_add(key))
    }

    /// With `measure_copies`, the resident bytes of the slot a value placed
    /// at `path` takes ([`crate::json::slot_bytes`]), measured before it is
    /// placed; 0 otherwise.
    fn slot_bytes(&self, path: &str) -> usize {
        if !self.measure_copies {
            return 0;
        }
        match pointer::split_last(path) {
            Some((parent, raw)) => {
                crate::json::slot_bytes(resolve_ref(self.doc, parent).ok(), &pointer::unescape(raw))
            }
            None => 0,
        }
    }

    /// The existing object member a non-root `path` names, if any.
    fn target_member(&self, path: &str) -> Option<&Value> {
        let (parent, raw) = pointer::split_last(path)?;
        match resolve_ref(self.doc, parent) {
            Ok(Value::Object(m)) => m.get(&*pointer::unescape(raw)),
            _ => None,
        }
    }
}
