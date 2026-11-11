//! Bounded, atomic RFC 6902 JSON Patch, written for this crate.
//!
//! Used for `STATE_DELTA` and `ACTIVITY_DELTA` on the client and for delta
//! computation on the server. Rules served: SNAP-05 (apply in order,
//! atomically), SNAP-08 / PROC-15 (operation objects are open: unknown members
//! survive), SNAP-09 (malformed operations are fatal, decided by enforcement
//! before this module sees them), SNAP-10 (unknown `op` dropped with a warning,
//! also by enforcement), SNAP-11..SNAP-14 (a failed or `test`-failed patch
//! leaves the document unchanged and is reported, not fatal).
//!
//! # Cost model
//!
//! * **Atomicity without cloning.** Operations are applied in place while an
//!   undo log records what each one displaced; on failure the log is replayed
//!   backwards, so the document is never cloned up front.
//! * **Work is bounded by the walk budget.** Everything an operation walks is
//!   charged to a per-patch walk budget of 4 x the growth allowance before
//!   the ceiling clamps it (or the document's size, if larger), so the
//!   memory ceiling never shrinks the work a patch may do: the values it
//!   clones or compares (including their text, see below), the subtree it displaces (`remove`, `replace`, a
//!   `copy` or `move` onto an existing member; counted only as far as the
//!   node budget would otherwise bind, and in full when the patch commits, so
//!   a frame that fails for another reason pays for its deficit at most, not
//!   the subtree), and the shifts below. A patch
//!   is therefore O(node budget), not O(operation size). A small patch that
//!   removes a subtree and then fails on another operation does not count it;
//!   one that must (budget binding, or committing) pays for it once.
//! * **Shifts are work.** Removing from or inserting into an array moves the
//!   later elements, and with `preserve-order` removing an object member is
//!   O(members). Both are charged to the walk budget at one unit per 16
//!   elements moved, so a patch does at most `16 x walk budget` element moves,
//!   linear in the node budget. Rollback replays only shifts already paid for
//!   (undoing an added member is O(1)). [`diff`] replaces an object (under
//!   `preserve-order`) or the whole document whenever the member-wise delta
//!   would cost more than this budget allows by default, and the server's
//!   state publisher sends that as a snapshot, so the crate never sends a
//!   delta its own default limits reject.
//! * **Nodes are node units.** Every JSON value counts one, and every 64
//!   bytes of string or object-key text counts one more (rounded down, a
//!   member's key and its string value separately), so the node budgets also
//!   bound the bytes a `copy` can duplicate. Each value, taken with its own
//!   member key and not counting its children, costs `c >= 1` units for at
//!   most `64c + 62` bytes of key and string text (a 63-byte key with a
//!   63-byte string costs one unit), so at most `126c`. Summed, a document
//!   of `n` units holds at most `126n` bytes of text (a copied subtree of
//!   `u` units at most `126u`): with the default 180,000 ceiling, at most
//!   22,680,000 bytes (about 22 MiB). Without object keys the bound is
//!   under `64n` bytes (11,520,000 at the default ceiling). What a unit
//!   costs in memory is on [`PatchLimits::max_nodes`]. The walk and growth allowances scale with the document's
//!   units, so they scale with its text as well as its node count.
//! * **Incremental budget.** A [`PatchBudget`] tracks the document's node
//!   count as operations add and remove subtrees, so
//!   a patch that would grow the document past
//!   `max(factor × nodes before, floor)` is refused without re-walking it.
//!   Whatever that allows, no patch takes a document past the absolute
//!   [`PatchLimits::max_nodes`], so repeated small patches cannot compound
//!   the growth ([`PatchErrorReason::NodeCeilingExceeded`]).
//! * **Pointer parsing** borrows from the operation and allocates only when a
//!   segment contains `~0`/`~1` escapes.

//!
//! # Semantics
//!
//! RFC 6902 §4 and RFC 6901, with these choices where the RFCs leave room:
//! `add`/`replace` at `""` replace the whole document, `remove` at `""` is
//! refused ([`PatchErrorReason::PathNotFound`]); array tokens are `-` or a
//! decimal without a leading zero, and `-` only names the append position;
//! `move` onto itself is a no-op whose source must exist; `move` into its own
//! child is refused, `copy` into it is allowed (the source is cloned first);
//! `test` compares numbers by exact value (`1` equals `1.0`; integers above
//! 2^53 do not equal the float they round to) and objects without
//! regard to member order.
//!
//! Rollback restores every value exactly. With the `preserve-order` feature,
//! removal shifts later members up (never swaps) and rollback re-inserts a
//! member at its original position, so member order is restored too; a
//! removal then costs O(members of that object).

use serde::de::{self, MapAccess, Visitor};
use serde::ser::SerializeMap;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::Value;

use crate::json::JsonObject;

mod engine;
mod pointer;
pub(crate) use pointer::push_token;
mod walk;

#[cfg(test)]
mod appendix_a;
#[cfg(test)]
mod tests;

/// An RFC 6901 JSON Pointer (`$defs/JsonPointer`), e.g. `/items/0/name`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct JsonPointer(String);

impl JsonPointer {
    /// Wraps pointer text without validating it; [`JsonPointer::parse`]
    /// validates. An invalid pointer built this way never reaches the wire:
    /// the producer verifier re-validates pointers
    /// ([`ProducerRule::InvalidValue`](crate::pipeline::verify::ProducerRule::InvalidValue)).
    pub fn new_unchecked(text: impl Into<String>) -> Self {
        JsonPointer(text.into())
    }

    /// Validates `^(/([^/~]|~[01])*)*$`.
    ///
    /// # Errors
    ///
    /// [`ErrorCode::InvalidValue`](crate::ErrorCode::InvalidValue).
    pub fn parse(text: impl Into<String>) -> crate::Result<Self> {
        let text = text.into();
        if pointer::is_valid(&text) {
            Ok(JsonPointer(text))
        } else {
            Err(crate::Error::new(
                crate::ErrorCode::InvalidValue,
                "invalid JSON pointer: expected \"\" or '/'-separated tokens with '~' only in ~0 or ~1",
            ))
        }
    }

    /// The pointer text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Unescaped reference tokens. Borrows unless a token has escapes.
    #[must_use]
    pub fn tokens(&self) -> Tokens<'_> {
        Tokens { rest: &self.0 }
    }
}

/// Iterator over a pointer's unescaped reference tokens, from [`JsonPointer::tokens`].
#[derive(Debug)]
pub struct Tokens<'a> {
    rest: &'a str,
}

impl<'a> Iterator for Tokens<'a> {
    type Item = std::borrow::Cow<'a, str>;

    fn next(&mut self) -> Option<Self::Item> {
        // `rest` is the unread text, starting at the separator of the next
        // token; the root pointer `""` has none.
        let body = self.rest.strip_prefix('/')?;
        let (token, rest) = match body.find('/') {
            Some(i) => (&body[..i], &body[i..]),
            None => (body, ""),
        };
        self.rest = rest;
        Some(pointer::unescape(token))
    }
}

/// The operation kind of a [`PatchOperation`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
#[non_exhaustive]
pub enum PatchOp {
    /// `add`.
    Add,
    /// `remove`.
    Remove,
    /// `replace`.
    Replace,
    /// `move`.
    Move,
    /// `copy`.
    Copy,
    /// `test`.
    Test,
}

/// One JSON Patch operation (`$defs/JsonPatchOperation`).
///
/// One struct for all six kinds keeps the wire order and the open members
/// intact; which of `from`/`value` is required depends on `op` and is checked
/// by enforcement (a missing or wrong-typed member is malformed and fatal,
/// SNAP-09). Members RFC 6902 does not define live in `extensions` and are
/// re-serialized unchanged (PROC-15, SNAP-08, PB-16).
///
/// `Serialize`/`Deserialize` are written by hand, not derived: known keys go
/// to the fields and every other key to `extensions` in one pass (no
/// `#[serde(flatten)]`, which buffers through serde's `Content`), and `value`
/// follows the "present" rule: `{"value": null}` is `Some(Value::Null)`, a
/// legitimate operand, while a missing `value` is `None` (malformed for
/// `add`/`replace`/`test`, SNAP-09). See [`crate::json`].
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub struct PatchOperation {
    /// The operation kind.
    pub op: PatchOp,
    /// Target location.
    pub path: JsonPointer,
    /// Source location, for `move` and `copy`. On any other op a string
    /// `from` is kept here and a `from` of any other type (`null` included)
    /// is an extension member (SNAP-08).
    pub from: Option<JsonPointer>,
    /// Operand, for `add`, `replace` and `test`. A present `null` is
    /// `Some(Value::Null)` and is written back as `null`.
    pub value: Option<Value>,
    /// Members RFC 6902 does not define, preserved.
    pub extensions: JsonObject,
}

impl Serialize for PatchOperation {
    /// `op`, `path`, then `from`/`value` when `Some` (a `Some(Null)` value is
    /// written as `null`), then `extensions`.
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let len = 2
            + usize::from(self.from.is_some())
            + usize::from(self.value.is_some())
            + self.extensions.len();
        let mut map = serializer.serialize_map(Some(len))?;
        map.serialize_entry("op", &self.op)?;
        map.serialize_entry("path", &self.path)?;
        if let Some(from) = &self.from {
            map.serialize_entry("from", from)?;
        }
        if let Some(value) = &self.value {
            map.serialize_entry("value", value)?;
        }
        for (key, value) in self.extensions.iter() {
            map.serialize_entry(key, value)?;
        }
        map.end()
    }
}

impl<'de> Deserialize<'de> for PatchOperation {
    /// One pass over the object's members; `value` by the present rule.
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_map(OperationVisitor)
    }
}

struct OperationVisitor;

impl<'de> Visitor<'de> for OperationVisitor {
    type Value = PatchOperation;

    fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("a JSON Patch operation object")
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<PatchOperation, A::Error> {
        let mut op = None;
        let mut path = None;
        let mut from_raw = None;
        let mut value = None;
        let mut extensions = JsonObject::new();
        while let Some(key) = map.next_key::<OperationKey>()? {
            match key {
                OperationKey::Op => op = Some(map.next_value::<PatchOp>()?),
                OperationKey::Path => path = Some(map.next_value::<JsonPointer>()?),
                // Typed once `op` is known: only `move`/`copy` define it.
                OperationKey::From => from_raw = Some(map.next_value::<Value>()?),
                // Present rule: `null` is an operand.
                OperationKey::Value => value = Some(map.next_value::<Value>()?),
                OperationKey::Other(key) => {
                    let v = map.next_value::<Value>()?;
                    extensions.insert(key, v);
                }
            }
        }
        let op = op.ok_or_else(|| de::Error::missing_field("op"))?;
        let path = path.ok_or_else(|| de::Error::missing_field("path"))?;
        let from = match from_raw {
            // A string is a pointer on every op, as RFC 6902 spells it.
            Some(Value::String(s)) => Some(JsonPointer(s)),
            // On `move`/`copy`, `null` reads as absent, which apply reports
            // as missing; any other type is malformed (SNAP-09).
            None | Some(Value::Null) if matches!(op, PatchOp::Move | PatchOp::Copy) => None,
            Some(other) if matches!(op, PatchOp::Move | PatchOp::Copy) => {
                return Err(de::Error::invalid_type(
                    unexpected(&other),
                    &"a JSON pointer string",
                ));
            }
            // On any other op `from` is undefined: kept whole, `null`
            // included (SNAP-08).
            Some(other) => {
                extensions.insert("from".to_owned(), other);
                None
            }
            None => None,
        };
        Ok(PatchOperation {
            op,
            path,
            from,
            value,
            extensions,
        })
    }
}

/// `value`'s type, for a serde type error.
fn unexpected(value: &Value) -> de::Unexpected<'_> {
    match value {
        Value::Null => de::Unexpected::Unit,
        Value::Bool(b) => de::Unexpected::Bool(*b),
        Value::Number(n) => match (n.as_u64(), n.as_i64(), n.as_f64()) {
            (Some(u), _, _) => de::Unexpected::Unsigned(u),
            (_, Some(i), _) => de::Unexpected::Signed(i),
            (_, _, Some(f)) => de::Unexpected::Float(f),
            _ => de::Unexpected::Other("number"),
        },
        Value::String(s) => de::Unexpected::Str(s),
        Value::Array(_) => de::Unexpected::Seq,
        Value::Object(_) => de::Unexpected::Map,
    }
}

/// A member name of a patch operation. The defined members are matched
/// without allocating (serde's `Cow<str>` always deserializes owned); only
/// an extension member's name is copied.
enum OperationKey {
    Op,
    Path,
    From,
    Value,
    Other(String),
}

impl<'de> Deserialize<'de> for OperationKey {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_identifier(OperationKeyVisitor)
    }
}

struct OperationKeyVisitor;

impl<'de> Visitor<'de> for OperationKeyVisitor {
    type Value = OperationKey;

    fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("a member name")
    }

    fn visit_str<E: de::Error>(self, key: &str) -> Result<OperationKey, E> {
        Ok(match key {
            "op" => OperationKey::Op,
            "path" => OperationKey::Path,
            "from" => OperationKey::From,
            "value" => OperationKey::Value,
            other => OperationKey::Other(other.to_owned()),
        })
    }

    fn visit_string<E: de::Error>(self, key: String) -> Result<OperationKey, E> {
        match key.as_str() {
            "op" | "path" | "from" | "value" => self.visit_str(&key),
            _ => Ok(OperationKey::Other(key)),
        }
    }
}

impl PatchOperation {
    fn with(
        op: PatchOp,
        path: JsonPointer,
        from: Option<JsonPointer>,
        value: Option<Value>,
    ) -> Self {
        PatchOperation {
            op,
            path,
            from,
            value,
            extensions: JsonObject::new(),
        }
    }

    /// `add` at `path`.
    #[must_use]
    pub fn add(path: JsonPointer, value: Value) -> Self {
        Self::with(PatchOp::Add, path, None, Some(value))
    }

    /// `remove` at `path`.
    #[must_use]
    pub fn remove(path: JsonPointer) -> Self {
        Self::with(PatchOp::Remove, path, None, None)
    }

    /// `replace` at `path`.
    #[must_use]
    pub fn replace(path: JsonPointer, value: Value) -> Self {
        Self::with(PatchOp::Replace, path, None, Some(value))
    }

    /// `move` from `from` to `path`.
    #[must_use]
    pub fn move_to(from: JsonPointer, path: JsonPointer) -> Self {
        Self::with(PatchOp::Move, path, Some(from), None)
    }

    /// `copy` from `from` to `path`.
    #[must_use]
    pub fn copy_to(from: JsonPointer, path: JsonPointer) -> Self {
        Self::with(PatchOp::Copy, path, Some(from), None)
    }

    /// `test` that `path` equals `value`.
    #[must_use]
    pub fn test(path: JsonPointer, value: Value) -> Self {
        Self::with(PatchOp::Test, path, None, Some(value))
    }
}

/// Hard ceiling on [`PatchLimits::max_depth`] for `apply` and its `test`
/// walks, whatever the caller configures. `diff` is not bounded by it: it
/// recurses only where both sides are objects, so its depth is that of its
/// inputs.
const DEPTH_CEILING: usize = 1024;

/// Walk budget per patch, as a multiple of the growth allowance. Everything the
/// cost model in the module docs lists as charged to the walk budget draws on
/// it: pointer tokens, clones, `test` compares, displaced-subtree counts and
/// array or object shifts.
const WALK_FACTOR: usize = 4;

/// The node cap and walk cap of a patch against a document of `nodes` nodes.
/// The walk cap follows the growth allowance before the ceiling clamps it, so
/// the memory ceiling bounds memory and never shrinks the work a patch may do
/// (a low ceiling does not turn a patch that only rearranges a document into
/// one that fails). It never falls below the document's own size (times
/// [`WALK_FACTOR`]), so counting what is already resident is always
/// affordable even when the document exceeds the ceiling.
fn caps(limits: PatchLimits, nodes: usize, ceiling: usize) -> (usize, usize) {
    let allowance = limits.node_cap(nodes);
    let node_cap = allowance.min(ceiling);
    let walk_cap = allowance.max(nodes).saturating_mul(WALK_FACTOR);
    (node_cap, walk_cap)
}

/// Resource limits for one document's patches.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct PatchLimits {
    /// Maximum pointer depth.
    pub max_depth: usize,
    /// Growth factor over the node units before the patch.
    pub growth_factor: usize,
    /// Node units the growth allowance never goes below. The per-patch walk
    /// budget is 4 times the allowance, so this is also the
    /// floor of the work any patch may do. Above `max_nodes` by default, so
    /// that only the ceiling refuses growth and the walk budget is the same
    /// as with a larger ceiling.
    pub min_nodes: usize,
    /// Absolute ceiling on node units, whatever the growth allowance: a patch
    /// never leaves a document larger ([`PatchErrorReason::NodeCeilingExceeded`]).
    /// The [`Reducer`](crate::pipeline::reduce::Reducer) applies it to the
    /// state and every patched activity content together, and the
    /// [`Processor`](crate::pipeline::Processor) ends the stream with
    /// `processing.limit` when a patch reaches it.
    ///
    /// Memory: a node unit costs from 32 bytes (a scalar array element) to
    /// about 690 bytes requested, about 720 bytes with allocator size
    /// classes (an object of one member with a key of up to 63 bytes, which
    /// costs no extra unit: a whole B-tree leaf plus the key's block; about
    /// 420 bytes with `preserve-order`), typically about 140 bytes for
    /// objects with short keys. The default 180,000 therefore bounds the
    /// patched state and activity content at about 124 MiB, and with the
    /// default history limit a stream's worst case near 252 MiB
    /// (docs/DECISIONS.md D14).
    ///
    /// An application with larger state raises this: each extra unit may
    /// hold up to about 720 bytes (1,000,000 units allow up to about
    /// 690 MiB). The ceiling does not change the per-patch walk budget,
    /// which follows the document's size. Raise
    /// [`Limits::max_container_entries`](crate::pipeline::Limits::max_container_entries)
    /// with it, below this value, to admit a `STATE_SNAPSHOT` of that size
    /// ([`Limits`](crate::pipeline::Limits), "Memory, and large state").
    pub max_nodes: usize,
}

impl Default for PatchLimits {
    /// Depth 128, factor 4, floor 1,000,000 node units (the walk budget's
    /// floor: 4,000,000 units of work per patch), ceiling 180,000 node units.
    fn default() -> Self {
        PatchLimits {
            max_depth: 128,
            growth_factor: 4,
            min_nodes: 1_000_000,
            max_nodes: 180_000,
        }
    }
}

impl PatchLimits {
    fn depth(&self) -> usize {
        self.max_depth.min(DEPTH_CEILING)
    }

    fn node_cap(&self, before: usize) -> usize {
        self.min_nodes
            .max(self.growth_factor.saturating_mul(before))
            .max(before)
    }
}

/// Incremental node accounting for one patched document (state, or one
/// activity message's content). Created from the document once; kept up to
/// date by [`apply`] so later patches never re-walk the document.
#[derive(Debug, Clone)]
pub struct PatchBudget {
    limits: PatchLimits,
    nodes: usize,
}

impl PatchBudget {
    /// Counts `document` once (O(document)); call again only after a
    /// snapshot replaces it.
    #[must_use]
    pub fn for_document(document: &Value, limits: PatchLimits) -> Self {
        PatchBudget {
            limits,
            nodes: walk::count(document),
        }
    }

    /// Current node units: one per value plus one per 64 bytes of string or
    /// key text (rounded down).
    #[must_use]
    pub fn nodes(&self) -> usize {
        self.nodes
    }
}

/// Why a patch did not apply. Not an [`Error`](crate::Error): a failed patch is
/// reported as a `patch.not_applied` diagnostic and the prior value is kept
/// (SNAP-12, SNAP-13).
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct PatchError {
    /// Index of the operation that failed.
    pub index: usize,
    /// What went wrong.
    pub reason: PatchErrorReason,
}

/// Classification of a [`PatchError`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum PatchErrorReason {
    /// The path (or `from`) does not resolve.
    PathNotFound,
    /// An array index is invalid.
    InvalidIndex,
    /// A `test` operation failed (SNAP-14).
    TestFailed,
    /// `move` into its own child.
    MoveIntoChild,
    /// Pointer deeper than [`PatchLimits::max_depth`].
    TooDeep,
    /// Result would exceed the node budget.
    BudgetExceeded,
    /// Result would exceed the absolute [`PatchLimits::max_nodes`] ceiling.
    NodeCeilingExceeded,
    /// A required member (`value`, `from`) is missing; normally caught by enforcement.
    MissingMember,
}

impl PatchErrorReason {
    /// Short, value-free description used in messages and diagnostics.
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            PatchErrorReason::PathNotFound => "path does not resolve",
            PatchErrorReason::InvalidIndex => "invalid or out-of-range array index",
            PatchErrorReason::TestFailed => "test operation failed",
            PatchErrorReason::MoveIntoChild => "cannot move a value into its own child",
            PatchErrorReason::TooDeep => "result would exceed the depth limit",
            PatchErrorReason::BudgetExceeded => "result would exceed the node budget",
            PatchErrorReason::NodeCeilingExceeded => {
                "result would exceed the absolute node ceiling"
            }
            PatchErrorReason::MissingMember => "required member missing",
        }
    }
}

impl std::fmt::Display for PatchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "JSON Patch operation {} not applied: {}",
            self.index,
            self.reason.as_str()
        )
    }
}

impl std::error::Error for PatchError {}

/// Outcome of [`apply_checked`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CheckedError {
    /// An operation failed.
    Patch(PatchError),
    /// Every operation applied, but the result failed the caller's check.
    Refused,
}

/// [`apply`], plus a check of the result before it is committed: when
/// `accept` returns `false` the document and budget are restored, exactly as
/// for a failing operation. Used to keep activity content an object (ACT-15).
///
/// `ceiling` lowers [`PatchLimits::max_nodes`] for this document (the
/// reducer passes what the other patched documents leave of it).
pub(crate) fn apply_checked(
    document: &mut Value,
    operations: &[PatchOperation],
    budget: &mut PatchBudget,
    ceiling: usize,
    accept: impl FnOnce(&Value) -> bool,
) -> Result<(), CheckedError> {
    apply_measured(document, operations, budget, ceiling, false, accept).map(|_| ())
}

/// What [`apply_measured`] measured of a patch that applied (all zero or
/// empty unless measuring).
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Measured {
    /// Estimated resident size of the values its `copy` operations
    /// duplicated and of the slots its `add`, `copy` and `move` operations
    /// placed values in.
    pub(crate) copied: usize,
    /// The slots its `remove` operations and `move` sources vacated, as
    /// (operation index, bytes), each what placing a value back there would
    /// have charged.
    pub(crate) vacated: Vec<(usize, usize)>,
}

/// [`apply_checked`] that, with `measure_copies`, also returns what the
/// patch placed and vacated ([`Measured`]): the only place that knows them,
/// since a later operation of the same patch may remove or move a copy's
/// source, and a container's length when a slot is vacated depends on the
/// operations before it.
pub(crate) fn apply_measured(
    document: &mut Value,
    operations: &[PatchOperation],
    budget: &mut PatchBudget,
    ceiling: usize,
    measure_copies: bool,
    accept: impl FnOnce(&Value) -> bool,
) -> Result<Measured, CheckedError> {
    let limits = budget.limits;
    let ceiling = ceiling.min(limits.max_nodes);
    let (node_cap, walk_cap) = caps(limits, budget.nodes, ceiling);
    let engine = engine::Engine::new(
        document,
        budget.nodes,
        engine::Caps {
            nodes: node_cap,
            ceiling,
            walk: walk_cap,
            depth: limits.depth(),
            measure_copies,
        },
    );
    match engine.run(operations, accept) {
        engine::Outcome::Applied {
            nodes,
            copied,
            vacated,
        } => {
            budget.nodes = nodes;
            Ok(Measured { copied, vacated })
        }
        engine::Outcome::Failed { index, reason } => {
            Err(CheckedError::Patch(PatchError { index, reason }))
        }
        engine::Outcome::Refused => Err(CheckedError::Refused),
    }
}

/// Applies `operations` to `document` in order, atomically: either every
/// operation applies, or `document` is exactly as before (SNAP-05, SNAP-11).
///
/// Extension members are ignored when applying and never cause failure.
///
/// # Errors
///
/// [`PatchError`] naming the first operation that failed; the document and the
/// budget are unchanged.
///
/// ```
/// use agui_sdk::patch::{apply, JsonPointer, PatchBudget, PatchLimits, PatchOperation};
/// use serde_json::json;
///
/// let mut state = json!({ "items": ["a"], "done": false });
/// let mut budget = PatchBudget::for_document(&state, PatchLimits::default());
///
/// let ops = [
///     PatchOperation::add(JsonPointer::parse("/items/-")?, json!("b")),
///     PatchOperation::replace(JsonPointer::parse("/done")?, json!(true)),
/// ];
/// apply(&mut state, &ops, &mut budget).expect("both operations apply");
/// assert_eq!(state, json!({ "items": ["a", "b"], "done": true }));
///
/// // Atomic: a failing `test` leaves the document exactly as it was.
/// let before = state.clone();
/// let ops = [
///     PatchOperation::remove(JsonPointer::parse("/items/0")?),
///     PatchOperation::test(JsonPointer::parse("/done")?, json!(false)),
/// ];
/// assert!(apply(&mut state, &ops, &mut budget).is_err());
/// assert_eq!(state, before);
/// # Ok::<(), agui_sdk::Error>(())
/// ```
pub fn apply(
    document: &mut Value,
    operations: &[PatchOperation],
    budget: &mut PatchBudget,
) -> Result<(), PatchError> {
    apply_checked(document, operations, budget, usize::MAX, |_| true).map_err(|e| match e {
        CheckedError::Patch(e) => e,
        // The check above accepts everything.
        CheckedError::Refused => PatchError {
            index: operations.len().saturating_sub(1),
            reason: PatchErrorReason::PathNotFound,
        },
    })
}

/// Computes a patch that turns `before` into `after`, for the server's state
/// publisher. Emits `replace` for changed scalars, `add`/`remove` for
/// object members and a whole-array `replace` when arrays differ (simple,
/// predictable, O(size of both documents plus the walk cost of the emitted
/// delta, which is capped)). With `preserve-order`, an object
/// whose deleted members would cost more to shift out than to rebuild is sent
/// as one `replace` of the object. Returns an empty patch when the documents
/// are equal. When the member-wise delta would exceed what the default
/// [`PatchLimits`] allow [`apply`] to spend (the walk budget, or growth past
/// the node budget or ceiling), returns a single `replace` of the root with
/// `after` instead. That replace is larger than `after` itself, so callers
/// building their own `STATE_DELTA` should send `after` as a `STATE_SNAPSHOT`
/// in that case, as the server's state publisher does.
#[must_use]
pub fn diff(before: &Value, after: &Value) -> Vec<PatchOperation> {
    let limits = PatchLimits::default();
    let nodes = walk::count(before);
    let (node_cap, walk_cap) = caps(limits, nodes, limits.max_nodes);
    let mut cost = Cost {
        walk: 0,
        grow: 0,
        limit: walk_cap,
    };
    let mut out = Vec::new();
    let mut path = String::new();
    diff_into(before, after, &mut path, &mut out, &mut cost);
    // Growth only counts when there is some: equal documents, and pure
    // removals, are accepted by `apply` even over the ceiling.
    if cost.walk > walk_cap || (cost.grow > 0 && nodes.saturating_add(cost.grow) > node_cap) {
        out.clear();
        out.push(PatchOperation::replace(
            JsonPointer(String::new()),
            after.clone(),
        ));
    }
    out
}

/// Upper bound of what [`apply`] charges for the operations `diff` has
/// emitted so far (ignoring lazy credit), so `diff` can tell when the default
/// limits would refuse its delta.
struct Cost {
    walk: usize,
    grow: usize,
    /// Stop estimating once `walk` exceeds this: the delta is refused anyway.
    limit: usize,
}

impl Cost {
    fn over(&self) -> bool {
        self.walk > self.limit
    }

    fn add(&mut self, path: &str, added: usize, removed: usize, shift: usize) {
        self.walk = self
            .walk
            .saturating_add(pointer::depth(path))
            .saturating_add(added)
            .saturating_add(removed)
            .saturating_add(shift);
        self.grow = self.grow.saturating_add(added);
    }
}

/// With `preserve-order`, each `remove` of an object member shifts the later
/// members, which [`apply`] charges to the walk budget. When that would cost
/// more than rebuilding the object, the delta replaces it whole (and
/// [`diff`] replaces the whole document when the delta as a whole would cost
/// more than the default limits allow), so the crate never emits a delta its
/// own default limits refuse. Always `false`
/// without `preserve-order` (removal is O(log n) there).
#[cfg(feature = "preserve-order")]
fn removals_too_costly(
    before: &serde_json::Map<String, Value>,
    after: &serde_json::Map<String, Value>,
) -> bool {
    let removed = before.keys().filter(|k| !after.contains_key(*k)).count();
    let shifts = removed.saturating_mul(engine::shift_cost(before.len()));
    // Cheap exit: rebuilding costs at least one unit per member.
    if shifts <= before.len() {
        return false;
    }
    // Rebuilding costs `before.len()` plus the nodes of `after`: count them
    // only as far as that could still be cheaper than `shifts`.
    let mut remaining = shifts - before.len();
    for v in after.values() {
        match walk::count_displaced(v, remaining) {
            Some(n) if n < remaining => remaining -= n,
            _ => return false,
        }
    }
    true
}

#[cfg(not(feature = "preserve-order"))]
fn removals_too_costly(
    _: &serde_json::Map<String, Value>,
    _: &serde_json::Map<String, Value>,
) -> bool {
    false
}

fn diff_into(
    before: &Value,
    after: &Value,
    path: &mut String,
    out: &mut Vec<PatchOperation>,
    cost: &mut Cost,
) {
    if cost.over() {
        return;
    }
    match (before, after) {
        (Value::Object(b), Value::Object(a)) => {
            if removals_too_costly(b, a) {
                cost.add(path, walk::count(after), walk::count(before), 0);
                out.push(PatchOperation::replace(
                    JsonPointer(path.clone()),
                    after.clone(),
                ));
                return;
            }
            let len = path.len();
            for (key, old) in b {
                pointer::push_token(path, key);
                match a.get(key) {
                    Some(new) => diff_into(old, new, path, out, cost),
                    None => {
                        cost.add(path, 0, walk::count(old), engine::remove_shift(b.len()));
                        out.push(PatchOperation::remove(JsonPointer(path.clone())));
                    }
                }
                path.truncate(len);
            }
            for (key, new) in a {
                if !b.contains_key(key) {
                    pointer::push_token(path, key);
                    // The member's own key is text the document retains.
                    let key_units = walk::text_units(key.len());
                    cost.add(path, walk::count(new).saturating_add(key_units), 0, 0);
                    out.push(PatchOperation::add(JsonPointer(path.clone()), new.clone()));
                    path.truncate(len);
                }
            }
        }
        _ if before == after => {}
        _ => {
            cost.add(path, walk::count(after), walk::count(before), 0);
            out.push(PatchOperation::replace(
                JsonPointer(path.clone()),
                after.clone(),
            ));
        }
    }
}
