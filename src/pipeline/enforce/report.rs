//! What enforcement produces besides the value: faults (fatal), findings
//! (non-fatal, become diagnostics), and the location bookkeeping both need.
//!
//! Locations are a chain of [`Path`] nodes living on the walker's stack, so
//! tracking where we are costs no allocation; a JSON pointer string is only
//! built when something is actually reported.

use std::fmt::{self, Write as _};

use crate::diagnostics::DiagnosticCode;

/// How a walk treats unrecognised material and budgets.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Options {
    /// Reject unrecognised material instead of stripping it (the strict
    /// schema, SCHF-01). Never used on a receive path.
    pub(crate) strict: bool,
    /// Read `null` as absent in the optional positions the compatibility
    /// boundary's list names, the shim for shipped producers (BASIC-12);
    /// a `null` anywhere else stays a type mismatch. Off for strict
    /// validation and for the value corpus.
    pub(crate) null_as_absent: bool,
    /// Object members plus array elements allowed in one document.
    pub(crate) max_entries: usize,
    /// Deepest container nesting allowed in one document.
    pub(crate) max_depth: usize,
    /// Findings kept per document before the rest are only counted.
    pub(crate) max_findings: usize,
}

impl Options {
    /// Lenient options with the default budgets.
    pub(crate) fn lenient(null_as_absent: bool) -> Self {
        let d = crate::pipeline::Limits::default();
        Options {
            strict: false,
            null_as_absent,
            max_entries: d.max_container_entries,
            max_depth: d.max_depth,
            max_findings: d.max_diagnostics_per_event,
        }
    }

    /// Strict-schema options with the default budgets.
    pub(crate) fn strict() -> Self {
        Options {
            strict: true,
            ..Options::lenient(false)
        }
    }
}

/// One step of a location.
#[derive(Debug, Clone, Copy)]
pub(crate) enum Seg<'a> {
    Key(&'a str),
    Index(usize),
}

/// A location in the document, as a chain of steps back to the root.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Path<'a> {
    parent: Option<&'a Path<'a>>,
    seg: Option<Seg<'a>>,
}

impl<'a> Path<'a> {
    /// The document root.
    pub(crate) const ROOT: Path<'static> = Path {
        parent: None,
        seg: None,
    };

    /// The member `key` of the value here.
    pub(crate) fn key(&'a self, key: &'a str) -> Path<'a> {
        Path {
            parent: Some(self),
            seg: Some(Seg::Key(key)),
        }
    }

    /// The element `index` of the array here.
    pub(crate) fn index(&'a self, index: usize) -> Path<'a> {
        Path {
            parent: Some(self),
            seg: Some(Seg::Index(index)),
        }
    }

    /// The RFC 6901 pointer to this location (`""` for the root), built in
    /// one allocation of its exact length.
    pub(crate) fn pointer(&self) -> String {
        let mut out = String::with_capacity(self.pointer_len());
        self.write(&mut out);
        out
    }

    /// The length of [`pointer`](Self::pointer)'s text.
    fn pointer_len(&self) -> usize {
        let mut len = 0_usize;
        let mut at = Some(self);
        while let Some(path) = at {
            len = len.saturating_add(match path.seg {
                None => 0,
                Some(Seg::Index(mut i)) => {
                    let mut digits = 1;
                    while i >= 10 {
                        i /= 10;
                        digits += 1;
                    }
                    1 + digits
                }
                // `~` and `/` are escaped as two characters each.
                Some(Seg::Key(k)) => k
                    .bytes()
                    .filter(|&b| b == b'~' || b == b'/')
                    .count()
                    .saturating_add(1 + k.len()),
            });
            at = path.parent;
        }
        len
    }

    fn write(&self, out: &mut String) {
        if let Some(parent) = self.parent {
            parent.write(out);
        }
        match self.seg {
            None => {}
            Some(Seg::Index(i)) => {
                let _ = write!(out, "/{i}");
            }
            Some(Seg::Key(k)) => {
                crate::patch::push_token(out, k);
            }
        }
    }
}

/// The class of a fatal verdict.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FaultKind {
    /// Not JSON, or not an event at all (no object, no `type`).
    Malformed,
    /// A described position holds a value the schema rejects (PROC-04).
    Invalid,
    /// A container-entry or depth budget was exceeded.
    Limit,
}

/// A fatal verdict with its location and a message free of payload values.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Fault {
    pub(crate) kind: FaultKind,
    pub(crate) pointer: String,
    pub(crate) message: String,
}

impl Fault {
    /// A schema-rejected value at `path`.
    pub(crate) fn invalid(path: &Path<'_>, detail: fmt::Arguments<'_>) -> Self {
        let pointer = path.pointer();
        let message = if pointer.is_empty() {
            format!("invalid value at the document root: {detail}")
        } else {
            format!(
                "invalid value at {}: {detail}",
                crate::diagnostics::bounded_pointer(&pointer)
            )
        };
        Fault {
            kind: FaultKind::Invalid,
            pointer,
            message,
        }
    }

    /// A value of the wrong JSON type at `path`.
    pub(crate) fn mismatch(path: &Path<'_>, expected: &dyn fmt::Display, got: Kind) -> Self {
        Fault::invalid(path, format_args!("expected {expected}, received {got}"))
    }

    /// Bytes that are not JSON.
    pub(crate) fn syntax(offset: usize, what: &str) -> Self {
        Fault {
            kind: FaultKind::Malformed,
            pointer: String::new(),
            message: format!("invalid JSON at byte {offset}: {what}"),
        }
    }

    /// A frame that is JSON but not an event.
    pub(crate) fn not_an_event(what: &str) -> Self {
        Fault {
            kind: FaultKind::Malformed,
            pointer: String::new(),
            message: format!("malformed event: {what}"),
        }
    }

    /// A budget exceeded at `path`.
    pub(crate) fn limit(path: &Path<'_>, what: fmt::Arguments<'_>) -> Self {
        Fault {
            kind: FaultKind::Limit,
            pointer: path.pointer(),
            message: format!("limit exceeded: {what}"),
        }
    }

    pub(crate) fn entries(path: &Path<'_>, max: usize) -> Self {
        Fault::limit(path, format_args!("more than {max} container entries"))
    }

    pub(crate) fn depth(path: &Path<'_>, max: usize) -> Self {
        Fault::limit(path, format_args!("nesting deeper than {max}"))
    }
}

/// The JSON type of a value, for messages.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Kind {
    Null,
    Bool,
    Number,
    String,
    Array,
    Object,
}

impl Kind {
    pub(crate) fn of(v: &serde_json::Value) -> Kind {
        use serde_json::Value;
        match v {
            Value::Null => Kind::Null,
            Value::Bool(_) => Kind::Bool,
            Value::Number(_) => Kind::Number,
            Value::String(_) => Kind::String,
            Value::Array(_) => Kind::Array,
            Value::Object(_) => Kind::Object,
        }
    }
}

impl fmt::Display for Kind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Kind::Null => "null",
            Kind::Bool => "boolean",
            Kind::Number => "number",
            Kind::String => "string",
            Kind::Array => "array",
            Kind::Object => "object",
        })
    }
}

/// A literal the schema pins, for `expected \"reasoning\"` messages. The
/// quotes are written escaped, as the conformance corpus pins the text
/// (`reasoning-message-wrong-role-fatal`).
pub(crate) struct Quoted(pub(crate) &'static str);

impl fmt::Display for Quoted {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "\\\"{}\\\"", self.0)
    }
}

/// `one of "a", "b"` for a closed set, quoted as [`Quoted`] quotes.
pub(crate) struct OneOf(pub(crate) &'static [&'static str]);

impl fmt::Display for OneOf {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("one of ")?;
        for (index, member) in self.0.iter().enumerate() {
            if index > 0 {
                f.write_str(", ")?;
            }
            write!(f, "{}", Quoted(member))?;
        }
        Ok(())
    }
}

/// Why unrecognised material was removed, carried up to where the removal
/// happens (the nearest optional field, list element or the root).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Removal {
    pub(crate) code: DiagnosticCode,
    /// The innermost required member that forced its container out.
    pub(crate) via: Option<&'static str>,
}

impl Removal {
    pub(crate) fn new(code: DiagnosticCode) -> Self {
        Removal { code, via: None }
    }

    /// Records that this removal propagated through required member `name`.
    pub(crate) fn through(self, name: &'static str) -> Self {
        Removal {
            via: self.via.or(Some(name)),
            ..self
        }
    }

    /// The diagnostic message.
    pub(crate) fn message(&self) -> String {
        let what = match self.code {
            DiagnosticCode::UnknownEnumValue => "unrecognised value of a closed set",
            DiagnosticCode::UnknownPatchOp => "JSON Patch operation with an unrecognised `op`",
            DiagnosticCode::UnknownEvent => "event of an unrecognised type",
            _ => "unrecognised union member",
        };
        match self.via {
            None => format!("{what} removed"),
            Some(name) => {
                format!("{what} in required member `{name}`; the value containing it was removed")
            }
        }
    }
}

/// One non-fatal finding, turned into a diagnostic by the caller.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Finding {
    pub(crate) code: DiagnosticCode,
    pub(crate) pointer: String,
    pub(crate) message: String,
}

/// The findings for one document, capped. Holds nothing (and allocates
/// nothing) for a conformant document.
///
/// Of the findings kept under the cap, only the first `deliver` are built:
/// the rest would reach no sink (the reporter is at its own cap, or its sink
/// is not enabled), so they are only counted (`muted`) and handed on as
/// such, which moves the reporter's counts exactly as delivered ones would.
#[derive(Debug, Default)]
pub(crate) struct Findings {
    pub(crate) items: Vec<Finding>,
    /// Findings kept under the cap but not built, after `items` in order.
    pub(crate) muted: usize,
    /// Findings beyond the cap, counted but not kept.
    pub(crate) suppressed: usize,
    /// Wire `type` of a dropped unknown event, for its diagnostic.
    pub(crate) unknown_type: Option<String>,
    cap: usize,
    deliver: usize,
}

impl Findings {
    /// Findings for a document, every kept one built.
    pub(crate) fn new(cap: usize) -> Self {
        Findings::delivering(cap, usize::MAX)
    }

    /// Findings building only the first `deliver` of those kept.
    pub(crate) fn delivering(cap: usize, deliver: usize) -> Self {
        Findings {
            items: Vec::new(),
            muted: 0,
            suppressed: 0,
            unknown_type: None,
            cap,
            deliver,
        }
    }

    /// Findings for a document whose diagnostics go to `reporter`: built
    /// only while it would deliver them.
    pub(crate) fn for_reporter(cap: usize, reporter: &crate::diagnostics::Reporter) -> Self {
        Findings::delivering(cap, reporter.room())
    }

    /// Empty findings with the same cap and delivery.
    pub(crate) fn fresh(&self) -> Self {
        Findings::delivering(self.cap, self.deliver)
    }

    /// Records a finding at `path`; the pointer and message are built only
    /// when it will be delivered.
    pub(crate) fn push(
        &mut self,
        code: DiagnosticCode,
        path: &Path<'_>,
        message: impl FnOnce() -> String,
    ) {
        if self.builds_next() {
            self.items.push(Finding {
                code,
                pointer: path.pointer(),
                message: message(),
            });
        } else {
            self.skip();
        }
    }

    /// Whether the next finding is kept and delivered, so worth building.
    pub(crate) fn builds_next(&self) -> bool {
        self.kept() < self.cap && self.items.len() < self.deliver
    }

    /// Counts the next finding without building it: muted under the cap,
    /// suppressed beyond it.
    pub(crate) fn skip(&mut self) {
        if self.kept() < self.cap {
            self.muted += 1;
        } else {
            self.suppressed += 1;
        }
    }

    fn kept(&self) -> usize {
        self.items.len().saturating_add(self.muted)
    }

    /// Records a finding whose pointer and message are already built, when
    /// [`builds_next`](Self::builds_next) said so.
    pub(crate) fn push_owned(&mut self, code: DiagnosticCode, pointer: String, message: String) {
        debug_assert!(self.builds_next(), "push_owned without builds_next");
        self.items.push(Finding {
            code,
            pointer,
            message,
        });
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.items.is_empty() && self.muted == 0 && self.suppressed == 0
    }

    /// Forgets everything (used when a document is re-walked from scratch).
    pub(crate) fn clear(&mut self) {
        self.items.clear();
        self.muted = 0;
        self.suppressed = 0;
        self.unknown_type = None;
    }
}

/// Whether `s` is an RFC 6901 JSON pointer: empty, or `/`-prefixed tokens in
/// which `~` is only ever followed by `0` or `1`.
pub(crate) fn is_pointer(s: &str) -> bool {
    let b = s.as_bytes();
    if !b.is_empty() && b[0] != b'/' {
        return false;
    }
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'~' && !matches!(b.get(i + 1), Some(b'0' | b'1')) {
            return false;
        }
        i += 1;
    }
    true
}

/// A JSON number as read.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum Num {
    U(u64),
    I(i64),
    F(f64),
}

impl Num {
    pub(crate) fn of(n: &serde_json::Number) -> Num {
        if let Some(u) = n.as_u64() {
            Num::U(u)
        } else if let Some(i) = n.as_i64() {
            Num::I(i)
        } else {
            Num::F(n.as_f64().unwrap_or(f64::NAN))
        }
    }
}

/// An integer accepted by an integer position, normalised (an integral
/// float such as `3.0` is the integer 3: JSON Schema's `integer`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum IntVal {
    U(u64),
    I(i64),
}

/// Checks `n` against an integer position. `Err` carries the reason.
pub(crate) fn check_int(n: Num, min: i64, max: i64) -> Result<IntVal, &'static str> {
    let wide: i128 = match n {
        Num::U(u) => i128::from(u),
        Num::I(i) => i128::from(i),
        Num::F(f) => {
            if !f.is_finite() || f.fract() != 0.0 {
                return Err("expected integer, received a fractional number");
            }
            if f.abs() > 1.0e19 {
                return Err("integer out of range");
            }
            // Integral and below 2^64 in magnitude, so the cast is exact.
            f as i128
        }
    };
    if wide < i128::from(min) || wide > i128::from(max) {
        return Err("integer out of range");
    }
    Ok(if wide >= 0 {
        IntVal::U(wide as u64)
    } else {
        IntVal::I(wide as i64)
    })
}
