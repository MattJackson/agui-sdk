//! Bounded walks over JSON values: measuring, cloning and comparing.
//!
//! [`measure`] and [`clone_bounded`] recurse at most `max_height` levels
//! below their start, so their stack use is bounded by the depth limit;
//! [`count`] and [`count_displaced`] switch to a heap stack below
//! `COUNT_RECURSION` levels; [`eq_bounded`] recurses as deep as the
//! shallower operand. Each stops as soon as a node bound is hit, so its cost
//! never exceeds what the budget allows.
//!
//! Cost is counted in node units: one per JSON value, plus one per
//! [`TEXT_BYTES_PER_UNIT`] bytes of string value or object key text, so the
//! bytes a walk clones or retains are bounded by the node budgets too
//! ([`measure`] is the exception: it relocates, it does not clone).

use serde_json::{Map, Number, Value};

/// Why a bounded walk stopped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Stop {
    /// More nodes than allowed.
    Nodes,
    /// Deeper than allowed.
    Depth,
}

/// Bytes of string or key text per extra node unit (floor division: text
/// under this length costs nothing beyond its value, and a member's key and
/// string value are rounded down separately). One value with its own member
/// key, not counting its children, costs `c >= 1` units for at most
/// `64c + 62 <= 126c` bytes of text, so a document or subtree of `n` units
/// holds at most `126n` bytes: 22,680,000 (about 22 MiB) at the default
/// 180,000-unit node ceiling.
const TEXT_BYTES_PER_UNIT: usize = 64;

/// Extra node units for `len` bytes of text.
#[inline]
pub(super) const fn text_units(len: usize) -> usize {
    len / TEXT_BYTES_PER_UNIT
}

/// Extra node units of a scalar: those of its text, if it is a string.
#[inline]
fn value_units(v: &Value) -> usize {
    match v {
        Value::String(s) => text_units(s.len()),
        _ => 0,
    }
}

struct Walk {
    nodes: usize,
    max_nodes: usize,
    max_height: usize,
}

impl Walk {
    #[inline]
    fn visit(&mut self, depth: usize, extra: usize) -> Result<(), Stop> {
        if self.nodes.saturating_add(extra) >= self.max_nodes {
            return Err(Stop::Nodes);
        }
        if depth > self.max_height {
            return Err(Stop::Depth);
        }
        self.nodes += 1 + extra;
        Ok(())
    }
}

/// Number of values in `value` (one unit each, text not weighted), stopping
/// early at either bound.
pub(super) fn measure(value: &Value, max_nodes: usize, max_height: usize) -> Result<usize, Stop> {
    fn go(v: &Value, w: &mut Walk, depth: usize) -> Result<(), Stop> {
        w.visit(depth, 0)?;
        match v {
            Value::Array(a) => a.iter().try_for_each(|x| go(x, w, depth + 1)),
            Value::Object(m) => m.values().try_for_each(|x| go(x, w, depth + 1)),
            _ => Ok(()),
        }
    }
    let mut w = Walk {
        nodes: 0,
        max_nodes,
        max_height,
    };
    go(value, &mut w, 0)?;
    Ok(w.nodes)
}

/// Node units of `value` (every JSON value counts one, plus its text, see the
/// module docs). Unbounded: used on
/// values already inside a document, or once when a budget is created.
///
/// Recursive (no allocation) for the first [`COUNT_RECURSION`] levels, which
/// covers every document the receive path admits; anything nested deeper
/// continues on an explicit heap stack, so a hand-built value of any depth
/// cannot exhaust the thread's stack.
pub(super) fn count(value: &Value) -> usize {
    fn go(v: &Value, depth: usize) -> usize {
        match v {
            Value::Array(a) if depth < COUNT_RECURSION => {
                1 + a.iter().map(|x| go(x, depth + 1)).sum::<usize>()
            }
            Value::Object(m) if depth < COUNT_RECURSION => {
                1 + m
                    .iter()
                    .map(|(k, x)| text_units(k.len()) + go(x, depth + 1))
                    .sum::<usize>()
            }
            Value::Array(_) | Value::Object(_) => count_deep(v),
            scalar => 1 + value_units(scalar),
        }
    }
    go(value, 0)
}

/// Levels [`count`] recurses before switching to a heap stack (the default
/// depth limit is 128).
const COUNT_RECURSION: usize = 256;

#[cold]
fn count_deep(value: &Value) -> usize {
    let mut n = 0;
    let mut stack = vec![(value, 0)];
    while let Some((v, key)) = stack.pop() {
        n += 1 + key + value_units(v);
        match v {
            Value::Array(a) => stack.extend(a.iter().map(|x| (x, 0))),
            Value::Object(m) => stack.extend(m.iter().map(|(k, x)| (x, text_units(k.len())))),
            _ => {}
        }
    }
    n
}

/// Node units of a displaced value, abandoned (`None`) as soon as it exceeds
/// `limit`. Never visits more than `limit + 1` nodes, and allocates nothing
/// for any value the receive path admits (recursive for the first
/// [`COUNT_RECURSION`] levels, a heap stack only below that).
pub(super) fn count_displaced(value: &Value, limit: usize) -> Option<usize> {
    fn go(v: &Value, n: &mut usize, limit: usize, depth: usize, key: usize) -> bool {
        if depth >= COUNT_RECURSION && matches!(v, Value::Array(_) | Value::Object(_)) {
            *n = n.saturating_add(key);
            if *n > limit {
                return false;
            }
            return match count_capped_deep(v, limit.saturating_sub(*n)) {
                Some(k) => {
                    *n += k;
                    true
                }
                None => false,
            };
        }
        *n = n.saturating_add(1 + key + value_units(v));
        if *n > limit {
            return false;
        }
        match v {
            Value::Array(a) => a.iter().all(|x| go(x, n, limit, depth + 1, 0)),
            Value::Object(m) => m
                .iter()
                .all(|(k, x)| go(x, n, limit, depth + 1, text_units(k.len()))),
            _ => true,
        }
    }
    let mut n = 0;
    go(value, &mut n, limit, 0, 0).then_some(n)
}

#[cold]
fn count_capped_deep(value: &Value, limit: usize) -> Option<usize> {
    let mut n = 0usize;
    let mut stack = vec![(value, 0)];
    while let Some((v, key)) = stack.pop() {
        n += 1 + key + value_units(v);
        // Every queued value is at least one more node.
        if n + stack.len() > limit {
            return None;
        }
        let width = match v {
            Value::Array(a) => a.len(),
            Value::Object(m) => m.len(),
            _ => 0,
        };
        if n + stack.len() + width > limit {
            return None;
        }
        match v {
            Value::Array(a) => stack.extend(a.iter().map(|x| (x, 0))),
            Value::Object(m) => stack.extend(m.iter().map(|(k, x)| (x, text_units(k.len())))),
            _ => {}
        }
    }
    Some(n)
}

/// Deep clone of `value`, abandoned (and the partial copy dropped) at either
/// bound. Returns the clone and its node units. Text is charged before it is
/// cloned, so a refused string or key is never allocated.
pub(super) fn clone_bounded(
    value: &Value,
    max_nodes: usize,
    max_height: usize,
) -> Result<(Value, usize), Stop> {
    fn go(v: &Value, w: &mut Walk, depth: usize, key: usize) -> Result<Value, Stop> {
        w.visit(depth, key + value_units(v))?;
        Ok(match v {
            Value::Array(a) => {
                let mut out = Vec::with_capacity(a.len());
                for x in a {
                    out.push(go(x, w, depth + 1, 0)?);
                }
                Value::Array(out)
            }
            Value::Object(m) => {
                let mut out = Map::new();
                for (k, x) in m {
                    // Charge the key (in `go`) before cloning it.
                    let child = go(x, w, depth + 1, text_units(k.len()))?;
                    out.insert(k.clone(), child);
                }
                Value::Object(out)
            }
            scalar => scalar.clone(),
        })
    }
    let mut w = Walk {
        nodes: 0,
        max_nodes,
        max_height,
    };
    let out = go(value, &mut w, 0, 0)?;
    Ok((out, w.nodes))
}

/// RFC 6902 §4.6 number equality: by exact numeric value, whatever the
/// representation (`1` equals `1.0`; `2^53 + 1` does not equal `2^53` as a float).
fn number_eq(a: &Number, b: &Number) -> bool {
    if let (Some(x), Some(y)) = (a.as_i64(), b.as_i64()) {
        return x == y;
    }
    if let (Some(x), Some(y)) = (a.as_u64(), b.as_u64()) {
        return x == y;
    }
    if a.is_f64() && b.is_f64() {
        return matches!((a.as_f64(), b.as_f64()), (Some(x), Some(y)) if x == y);
    }
    if a.is_f64() || b.is_f64() {
        // Exact: an integer above 2^53 must not round to a float to match it.
        let (f, int) = if a.is_f64() { (a, b) } else { (b, a) };
        let Some(f) = f.as_f64() else { return false };
        let Some(i) = int
            .as_i64()
            .map(i128::from)
            .or(int.as_u64().map(i128::from))
        else {
            return false;
        };
        return f.fract() == 0.0
            && (-9_223_372_036_854_775_808.0..18_446_744_073_709_551_616.0).contains(&f)
            && f as i128 == i;
    }
    // A negative i64 against a u64 above i64::MAX.
    false
}

/// RFC 6902 §4.6 equality of a document value `a` and an operation's
/// expected value `b`, charging `budget` in node units: one per node pair,
/// plus the text units of every key looked up and every equal-length string
/// compared. Objects are matched by looking `b`'s keys up in `a`, so only the
/// operation's own keys are hashed. `None` when the budget runs out.
pub(super) fn eq_bounded(a: &Value, b: &Value, budget: &mut usize) -> Option<bool> {
    *budget = budget.checked_sub(1)?;
    Some(match (a, b) {
        (Value::Null, Value::Null) => true,
        (Value::Bool(x), Value::Bool(y)) => x == y,
        (Value::Number(x), Value::Number(y)) => number_eq(x, y),
        (Value::String(x), Value::String(y)) => {
            if x.len() != y.len() {
                return Some(false);
            }
            *budget = budget.checked_sub(text_units(y.len()))?;
            x == y
        }
        (Value::Array(x), Value::Array(y)) => {
            if x.len() != y.len() {
                return Some(false);
            }
            for (p, q) in x.iter().zip(y) {
                if !eq_bounded(p, q, budget)? {
                    return Some(false);
                }
            }
            true
        }
        (Value::Object(x), Value::Object(y)) => {
            if x.len() != y.len() {
                return Some(false);
            }
            for (k, q) in y {
                *budget = budget.checked_sub(text_units(k.len()))?;
                match x.get(k) {
                    Some(p) if eq_bounded(p, q, budget)? => {}
                    _ => return Some(false),
                }
            }
            true
        }
        _ => false,
    })
}
