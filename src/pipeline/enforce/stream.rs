//! Enforcement straight from bytes, in the same pass that builds the typed
//! value.
//!
//! [`Node`] is a `serde::Deserializer` over the frame's bytes that knows the
//! shape of the position it stands on. The target type's own `Deserialize`
//! drives it, and sees only enforced material: undescribed members are
//! skipped before their key reaches the visitor, a `null` in an optional
//! position is skipped when the compatibility shim is on, an unrecognised
//! closed-set value or union member in an optional position or list is
//! skipped, and a malformed known value stops the parse with a [`Fault`].
//! Strings without escapes are lent to the visitor straight from the frame.
//!
//! One case cannot be settled in a single forward pass: an unrecognised
//! value in a *required* position removes its container, which the visitor
//! may already be half way through. That ends the pass with
//! [`DeErr::Reroute`], and the caller re-walks the frame as a `Value`
//! ([`super::tree`]). In lenient mode so does a repeated member, which the
//! value walk resolves as `JSON.parse` does, the last occurrence winning.
//! Unrecognised material in required positions is rare by construction (it
//! means a newer peer), so the common paths, conformant or merely carrying
//! extra members, stay single pass.
//!
//! Finding a union's discriminator looks ahead within that one object
//! without consuming it; unions nest only a fixed number of levels deep in
//! the schema, so this keeps the whole walk linear in the frame size.

use std::cell::Cell;
use std::fmt;

use serde::de::value::{BorrowedStrDeserializer, StrDeserializer};
use serde::de::{self, DeserializeSeed, IgnoredAny, MapAccess, SeqAccess, Visitor};
use serde::forward_to_deserialize_any;

use crate::diagnostics::DiagnosticCode;

use super::report::{
    check_int, is_pointer, Fault, Findings, IntVal, Kind, Num, OneOf, Options, Path, Quoted,
    Removal,
};
use super::shape::{Field, List, Obj, Shape, Union};

/// Why a pass stopped.
#[derive(Debug)]
pub(crate) enum DeErr {
    /// A fatal verdict.
    Fault(Fault),
    /// The frame needs the `Value` walk (see the module docs).
    Reroute,
    /// The target type rejected something on its own.
    Custom(String),
}

impl fmt::Display for DeErr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DeErr::Fault(fault) => f.write_str(&fault.message),
            DeErr::Reroute => f.write_str("frame needs the value walk"),
            DeErr::Custom(m) => f.write_str(m),
        }
    }
}

impl std::error::Error for DeErr {}

impl de::Error for DeErr {
    fn custom<T: fmt::Display>(msg: T) -> Self {
        DeErr::Custom(msg.to_string())
    }
}

impl From<Fault> for DeErr {
    fn from(f: Fault) -> Self {
        DeErr::Fault(f)
    }
}

type R<T> = Result<T, DeErr>;

thread_local! {
    /// Unescaping buffer, reused across frames so escaped strings cost no
    /// allocation once warm.
    static SCRATCH: Cell<String> = const { Cell::new(String::new()) };
}

/// Scratch kept between frames is capped so one huge frame does not pin
/// memory for the thread's lifetime.
const SCRATCH_KEEP: usize = 64 * 1024;

/// The end of the plain run of string bytes starting at `i`: the index of
/// the first `"`, `\\` or control character, or `b.len()`. Eight bytes per
/// step (SWAR on a `u64`, no `unsafe`); a set flag below the first match is
/// impossible, so the lowest flagged byte is the answer.
#[inline]
pub(super) fn plain_run(b: &[u8], mut i: usize) -> usize {
    const ONES: u64 = 0x0101_0101_0101_0101;
    const HIGH: u64 = 0x8080_8080_8080_8080;
    while let Some(word) = b.get(i..i + 8) {
        let Ok(word) = <[u8; 8]>::try_from(word) else {
            break;
        };
        let x = u64::from_le_bytes(word);
        let quote = x ^ (ONES * u64::from(b'"'));
        let backslash = x ^ (ONES * u64::from(b'\\'));
        let found = (quote.wrapping_sub(ONES) & !quote)
            | (backslash.wrapping_sub(ONES) & !backslash)
            | (x.wrapping_sub(ONES * 0x20) & !x);
        let found = found & HIGH;
        if found != 0 {
            return i + (found.trailing_zeros() / 8) as usize;
        }
        i += 8;
    }
    while let Some(&c) = b.get(i) {
        if c == b'"' || c == b'\\' || c < 0x20 {
            break;
        }
        i += 1;
    }
    i
}

/// A string just read: lent from the frame, or unescaped into scratch.
#[derive(Debug, Clone, Copy)]
enum Text<'de> {
    Borrowed(&'de str),
    Scratch,
}

/// A member name just read.
#[derive(Debug, Clone, Copy)]
enum Key<'de> {
    /// No escapes; not yet checked as UTF-8 (a name that matches a schema
    /// member is ASCII, so it never needs the check). The offset is for the
    /// error position.
    Plain(&'de [u8], usize),
    /// Decoded (and checked) by [`Stream::string`].
    Text(Text<'de>),
}

/// What a lookahead found at a union position.
enum Tag {
    NotObject(Kind),
    Missing,
    NotString(Kind),
    Known(&'static str, Obj),
    Unknown(Option<String>),
}

/// What a position's lookahead decided before its value is handed on.
enum Pre {
    /// Hand the value on; for a union, the variant already resolved.
    Go(Option<Obj>),
    /// Unrecognised: remove it (or its container).
    Unknown(Removal),
}

/// The reader and everything a walk accumulates.
pub(crate) struct Stream<'de, 'o> {
    b: &'de [u8],
    i: usize,
    scratch: String,
    opts: &'o Options,
    pub(crate) findings: &'o mut Findings,
    entries: usize,
    /// Bytes read twice by lookahead; the linearity test bounds it.
    pub(crate) rescanned: usize,
}

impl Drop for Stream<'_, '_> {
    fn drop(&mut self) {
        let mut scratch = std::mem::take(&mut self.scratch);
        if scratch.capacity() <= SCRATCH_KEEP {
            scratch.clear();
            SCRATCH.with(|s| s.set(scratch));
        }
    }
}

/// How a document came out of [`Stream::run`].
pub(crate) enum Rooted<T> {
    /// Enforced and built.
    Value(T),
    /// The root itself was unrecognised and dropped (with a finding).
    Dropped,
    /// Needs the `Value` walk; findings so far must be discarded.
    Reroute,
}

impl<'de, 'o> Stream<'de, 'o> {
    pub(crate) fn new(b: &'de [u8], opts: &'o Options, findings: &'o mut Findings) -> Self {
        Stream {
            b,
            i: 0,
            scratch: SCRATCH.with(Cell::take),
            opts,
            findings,
            entries: 0,
            rescanned: 0,
        }
    }

    /// Enforces the whole frame as `shape` and builds a `T` from it.
    pub(crate) fn run<T: de::Deserialize<'de>>(
        &mut self,
        shape: Shape,
    ) -> Result<Rooted<T>, Fault> {
        self.ws();
        match self.precheck(shape, 0) {
            Ok(Pre::Unknown(removal)) => {
                // `precheck` only says Unknown in lenient mode.
                let name = match shape {
                    Shape::Union(u) => match self.scan_tag(u, 0, true) {
                        Ok(Tag::Unknown(name)) => name,
                        _ => None,
                    },
                    _ => None,
                };
                self.skip_value(&Path::ROOT, 0, true).map_err(fault_of)?;
                self.end()?;
                super::report_root_drop(self.findings, removal, name.as_deref());
                Ok(Rooted::Dropped)
            }
            Ok(Pre::Go(hint)) => {
                let shape = hint.map_or(shape, Shape::Object);
                let node = Node {
                    st: self,
                    shape,
                    path: &Path::ROOT,
                    depth: 0,
                };
                match T::deserialize(node) {
                    Ok(t) => {
                        self.end()?;
                        Ok(Rooted::Value(t))
                    }
                    Err(DeErr::Reroute) => Ok(Rooted::Reroute),
                    Err(e) => Err(fault_of(e)),
                }
            }
            Err(DeErr::Reroute) => Ok(Rooted::Reroute),
            Err(e) => Err(fault_of(e)),
        }
    }

    /// The root union's discriminator, for the event-level checks that come
    /// before enforcement proper (`type` present, a non-empty string).
    pub(crate) fn root_tag(&mut self, u: Union) -> Result<RootTag, Fault> {
        self.ws();
        let tag = match self.scan_tag(u, 0, true) {
            Ok(tag) => tag,
            Err(DeErr::Reroute) => return Ok(RootTag::Reroute),
            Err(e) => return Err(fault_of(e)),
        };
        Ok(match tag {
            Tag::NotObject(_) => RootTag::NotObject,
            Tag::Missing => RootTag::Missing,
            Tag::NotString(_) => RootTag::NotString,
            Tag::Known(name, obj) => RootTag::Known(name, obj),
            Tag::Unknown(name) => match name {
                Some(n) if n.is_empty() => RootTag::Empty,
                _ => RootTag::Unknown,
            },
        })
    }

    fn end(&mut self) -> Result<(), Fault> {
        self.ws();
        if self.i != self.b.len() {
            return Err(Fault::syntax(self.i, "trailing characters after the value"));
        }
        Ok(())
    }

    // ---- lexing -------------------------------------------------------

    fn ws(&mut self) {
        while let Some(b' ' | b'\t' | b'\n' | b'\r') = self.b.get(self.i) {
            self.i += 1;
        }
    }

    fn peek(&self) -> Option<u8> {
        self.b.get(self.i).copied()
    }

    fn syntax(&self, what: &str) -> DeErr {
        DeErr::Fault(Fault::syntax(self.i, what))
    }

    /// The JSON type of the value starting here.
    fn kind(&self) -> R<Kind> {
        Ok(match self.peek() {
            Some(b'{') => Kind::Object,
            Some(b'[') => Kind::Array,
            Some(b'"') => Kind::String,
            Some(b't' | b'f') => Kind::Bool,
            Some(b'n') => Kind::Null,
            Some(b'-' | b'0'..=b'9') => Kind::Number,
            Some(_) => return Err(self.syntax("expected a value")),
            None => return Err(self.syntax("unexpected end of input")),
        })
    }

    fn expect(&mut self, byte: u8, what: &str) -> R<()> {
        if self.peek() == Some(byte) {
            self.i += 1;
            Ok(())
        } else {
            Err(self.syntax(what))
        }
    }

    fn text<'s>(&'s self, t: &Text<'de>) -> &'s str {
        match *t {
            Text::Borrowed(s) => s,
            Text::Scratch => &self.scratch,
        }
    }

    fn utf8(&self, bytes: &'de [u8], at: usize) -> R<&'de str> {
        std::str::from_utf8(bytes).map_err(|e| {
            DeErr::Fault(Fault::syntax(
                at + e.valid_up_to(),
                "invalid UTF-8 in string",
            ))
        })
    }

    /// Reads a string; the cursor is on its opening quote.
    fn string(&mut self) -> R<Text<'de>> {
        self.expect(b'"', "expected a string")?;
        let b = self.b;
        let start = self.i;
        self.i = plain_run(b, self.i);
        match b.get(self.i) {
            None => return Err(self.syntax("unterminated string")),
            Some(b'"') => {
                let s = self.utf8(&b[start..self.i], start)?;
                self.i += 1;
                return Ok(Text::Borrowed(s));
            }
            Some(b'\\') => {}
            Some(_) => return Err(self.syntax("control character in string")),
        }
        // Escapes: unescape into scratch.
        self.scratch.clear();
        let head = self.utf8(&b[start..self.i], start)?;
        self.scratch.push_str(head);
        loop {
            let run = self.i;
            self.i = plain_run(b, self.i);
            match b.get(self.i) {
                None => return Err(self.syntax("unterminated string")),
                Some(b'"' | b'\\') => {}
                Some(_) => return Err(self.syntax("control character in string")),
            }
            let chunk = self.utf8(&b[run..self.i], run)?;
            self.scratch.push_str(chunk);
            if b.get(self.i) == Some(&b'"') {
                self.i += 1;
                return Ok(Text::Scratch);
            }
            self.i += 1; // the backslash
            let esc = b.get(self.i).copied();
            self.i += 1;
            let c = match esc {
                Some(b'"') => '"',
                Some(b'\\') => '\\',
                Some(b'/') => '/',
                Some(b'b') => '\u{8}',
                Some(b'f') => '\u{c}',
                Some(b'n') => '\n',
                Some(b'r') => '\r',
                Some(b't') => '\t',
                Some(b'u') => self.unicode_escape()?,
                _ => return Err(self.syntax("invalid escape")),
            };
            self.scratch.push(c);
        }
    }

    /// Reads a member name, without the UTF-8 check when it has no escapes
    /// (see [`Key::Plain`]); the cursor is on its opening quote.
    fn key(&mut self) -> R<Key<'de>> {
        let at = self.i;
        self.expect(b'"', "expected a string")?;
        let b = self.b;
        let start = self.i;
        let end = plain_run(b, start);
        if b.get(end) == Some(&b'"') {
            self.i = end + 1;
            return Ok(Key::Plain(&b[start..end], start));
        }
        self.i = at;
        self.string().map(Key::Text)
    }

    /// A key as text, checking a plain one's UTF-8 now.
    fn key_text(&self, key: Key<'de>) -> R<Text<'de>> {
        match key {
            Key::Plain(bytes, at) => self.utf8(bytes, at).map(Text::Borrowed),
            Key::Text(t) => Ok(t),
        }
    }

    /// Whether `key` is exactly `name`.
    fn key_is(&self, key: &Key<'de>, name: &str) -> bool {
        match key {
            Key::Plain(bytes, _) => *bytes == name.as_bytes(),
            Key::Text(t) => self.text(t) == name,
        }
    }

    /// Steps over a string without decoding it: the lookahead's reader.
    /// Escapes are passed over, not interpreted, and UTF-8 is not checked:
    /// every byte a lookahead passes is read again, in full, by the walk
    /// that follows it (or by the skip that removes it).
    fn skip_string(&mut self) -> R<()> {
        self.expect(b'"', "expected a string")?;
        let b = self.b;
        loop {
            self.i = plain_run(b, self.i);
            match b.get(self.i) {
                None => return Err(self.syntax("unterminated string")),
                Some(b'"') => {
                    self.i += 1;
                    return Ok(());
                }
                // The escaped byte is never a terminator.
                Some(b'\\') => self.i += 2,
                Some(_) => return Err(self.syntax("control character in string")),
            }
        }
    }

    fn hex4(&mut self) -> R<u32> {
        let digits = self
            .b
            .get(self.i..self.i + 4)
            .ok_or_else(|| self.syntax("truncated \\u escape"))?;
        let mut n = 0u32;
        for d in digits {
            let v = (*d as char)
                .to_digit(16)
                .ok_or_else(|| self.syntax("invalid \\u escape"))?;
            n = n * 16 + v;
        }
        self.i += 4;
        Ok(n)
    }

    fn unicode_escape(&mut self) -> R<char> {
        let first = self.hex4()?;
        let code = match first {
            0xD800..=0xDBFF => {
                if self.b.get(self.i..self.i + 2) != Some(b"\\u") {
                    return Err(self.syntax("lone surrogate in \\u escape"));
                }
                self.i += 2;
                let second = self.hex4()?;
                if !(0xDC00..=0xDFFF).contains(&second) {
                    return Err(self.syntax("lone surrogate in \\u escape"));
                }
                0x10000 + ((first - 0xD800) << 10) + (second - 0xDC00)
            }
            0xDC00..=0xDFFF => return Err(self.syntax("lone surrogate in \\u escape")),
            n => n,
        };
        char::from_u32(code).ok_or_else(|| self.syntax("invalid \\u escape"))
    }

    fn number(&mut self) -> R<Num> {
        let b = self.b;
        let start = self.i;
        let digits = |s: &mut Self| {
            let from = s.i;
            while let Some(b'0'..=b'9') = b.get(s.i) {
                s.i += 1;
            }
            s.i - from
        };
        if b.get(self.i) == Some(&b'-') {
            self.i += 1;
        }
        let int_start = self.i;
        let n = digits(self);
        if n == 0 || (n > 1 && b[int_start] == b'0') {
            return Err(self.syntax("invalid number"));
        }
        let mut float = false;
        if b.get(self.i) == Some(&b'.') {
            self.i += 1;
            float = true;
            if digits(self) == 0 {
                return Err(self.syntax("invalid number"));
            }
        }
        if let Some(b'e' | b'E') = b.get(self.i) {
            self.i += 1;
            float = true;
            if let Some(b'+' | b'-') = b.get(self.i) {
                self.i += 1;
            }
            if digits(self) == 0 {
                return Err(self.syntax("invalid number"));
            }
        }
        let s =
            std::str::from_utf8(&b[start..self.i]).map_err(|_| self.syntax("invalid number"))?;
        if !float {
            if s.starts_with('-') {
                match s.parse::<i64>() {
                    // `-0` is the float negative zero, as serde_json reads it.
                    Ok(0) => return Ok(Num::F(-0.0)),
                    Ok(i) => return Ok(Num::I(i)),
                    Err(_) => {}
                }
            } else if let Ok(u) = s.parse::<u64>() {
                return Ok(Num::U(u));
            }
        }
        match s.parse::<f64>() {
            Ok(f) if f.is_finite() => Ok(Num::F(f)),
            _ => Err(DeErr::Fault(Fault::syntax(start, "number out of range"))),
        }
    }

    fn literal(&mut self, word: &[u8]) -> R<()> {
        if self.b.get(self.i..self.i + word.len()) == Some(word) {
            self.i += word.len();
            Ok(())
        } else {
            Err(self.syntax("invalid literal"))
        }
    }

    fn enter(&self, level: usize, path: &Path<'_>) -> R<()> {
        if level > self.opts.max_depth {
            return Err(Fault::depth(path, self.opts.max_depth).into());
        }
        Ok(())
    }

    fn add_entry(&mut self, path: &Path<'_>) -> R<()> {
        self.entries += 1;
        if self.entries > self.opts.max_entries {
            return Err(Fault::entries(path, self.opts.max_entries).into());
        }
        Ok(())
    }

    /// After a member or element: `,` (more follow) or the closing byte.
    /// Returns whether the container ended.
    fn after_item(&mut self, close: u8) -> R<bool> {
        self.ws();
        match self.peek() {
            Some(b',') => {
                self.i += 1;
                self.ws();
                Ok(false)
            }
            Some(c) if c == close => {
                self.i += 1;
                Ok(true)
            }
            _ => Err(self.syntax(if close == b'}' {
                "expected `,` or `}`"
            } else {
                "expected `,` or `]`"
            })),
        }
    }

    /// Steps over one value. `count` charges its entries to the budget and
    /// reads its strings in full; without it (a lookahead, whose bytes the
    /// walk reads again) strings are only stepped over. `depth` is the number
    /// of containers around it.
    fn skip_value(&mut self, path: &Path<'_>, depth: usize, count: bool) -> R<()> {
        match self.kind()? {
            Kind::Object => {
                self.enter(depth + 1, path)?;
                self.i += 1;
                self.ws();
                if self.peek() == Some(b'}') {
                    self.i += 1;
                    return Ok(());
                }
                loop {
                    if count {
                        self.add_entry(path)?;
                        self.string()?;
                    } else {
                        self.skip_string()?;
                    }
                    self.ws();
                    self.expect(b':', "expected `:`")?;
                    self.ws();
                    self.skip_value(path, depth + 1, count)?;
                    if self.after_item(b'}')? {
                        return Ok(());
                    }
                }
            }
            Kind::Array => {
                self.enter(depth + 1, path)?;
                self.i += 1;
                self.ws();
                if self.peek() == Some(b']') {
                    self.i += 1;
                    return Ok(());
                }
                loop {
                    if count {
                        self.add_entry(path)?;
                    }
                    self.skip_value(path, depth + 1, count)?;
                    if self.after_item(b']')? {
                        return Ok(());
                    }
                }
            }
            Kind::String if count => self.string().map(|_| ()),
            Kind::String => self.skip_string(),
            Kind::Number => self.number().map(|_| ()),
            Kind::Bool => {
                if self.peek() == Some(b't') {
                    self.literal(b"true")
                } else {
                    self.literal(b"false")
                }
            }
            Kind::Null => self.literal(b"null"),
        }
    }

    /// Looks ahead (without consuming) for the discriminator of the object
    /// starting here.
    fn scan_tag(&mut self, u: Union, depth: usize, want_name: bool) -> R<Tag> {
        let start = self.i;
        let found = self.scan_tag_inner(u, depth, want_name);
        self.rescanned += self.i.saturating_sub(start);
        self.i = start;
        found
    }

    fn scan_tag_inner(&mut self, u: Union, depth: usize, want_name: bool) -> R<Tag> {
        let kind = self.kind()?;
        if kind != Kind::Object {
            return Ok(Tag::NotObject(kind));
        }
        self.i += 1;
        self.ws();
        if self.peek() == Some(b'}') {
            return Ok(Tag::Missing);
        }
        let mut first = true;
        loop {
            let key = self.key()?;
            let is_tag = self.key_is(&key, u.tag);
            self.ws();
            self.expect(b':', "expected `:`")?;
            self.ws();
            if is_tag {
                let kind = self.kind()?;
                if kind != Kind::String {
                    return Ok(Tag::NotString(kind));
                }
                let value = self.string()?;
                let s = self.text(&value);
                return Ok(match u.variant(s) {
                    Some((name, mut obj)) => {
                        if !first {
                            obj.hoist = Some((u.tag, name));
                        }
                        Tag::Known(name, obj)
                    }
                    None => {
                        let name = want_name.then(|| s.to_owned());
                        if !self.opts.strict && self.tag_repeats(u, depth)? {
                            // A later occurrence wins on the value path, so
                            // this verdict would depend on the walk taken.
                            return Err(DeErr::Reroute);
                        }
                        Tag::Unknown(name)
                    }
                });
            }
            first = false;
            self.skip_value(&Path::ROOT, depth + 1, false)?;
            if self.after_item(b'}')? {
                return Ok(Tag::Missing);
            }
        }
    }

    /// Having read the first discriminator of an object, scans the rest of
    /// it for another one. Leaves the cursor wherever it stops.
    fn tag_repeats(&mut self, u: Union, depth: usize) -> R<bool> {
        if self.after_item(b'}')? {
            return Ok(false);
        }
        loop {
            let key = self.key()?;
            let is_tag = self.key_is(&key, u.tag);
            self.ws();
            self.expect(b':', "expected `:`")?;
            self.ws();
            if is_tag {
                return Ok(true);
            }
            self.skip_value(&Path::ROOT, depth + 1, false)?;
            if self.after_item(b'}')? {
                return Ok(false);
            }
        }
    }

    /// Decides, before a value is handed on, whether it is unrecognised.
    /// Only closed sets and unions can be; everything else is `Go`.
    fn precheck(&mut self, shape: Shape, depth: usize) -> R<Pre> {
        let unknown = match shape {
            Shape::Enum(set)
            | Shape::Pinned { set, .. }
            | Shape::Restricted { domain: set, .. } => {
                if self.peek() != Some(b'"') {
                    return Ok(Pre::Go(None));
                }
                let start = self.i;
                let t = self.string()?;
                let known = set.contains(&self.text(&t));
                self.rescanned += self.i - start;
                self.i = start;
                if known {
                    return Ok(Pre::Go(None));
                }
                Removal::new(DiagnosticCode::UnknownEnumValue)
            }
            Shape::Union(u) => match self.scan_tag(u, depth, false)? {
                Tag::Known(_, obj) => return Ok(Pre::Go(Some(obj))),
                Tag::Unknown(_) => Removal::new(u.unknown.code()),
                // Malformed: let the value walk report it precisely.
                _ => return Ok(Pre::Go(None)),
            },
            _ => return Ok(Pre::Go(None)),
        };
        if self.opts.strict {
            // Strict validation reports it where the value is walked.
            return Ok(Pre::Go(None));
        }
        Ok(Pre::Unknown(unknown))
    }

    fn visit_text<V: Visitor<'de>>(&self, t: Text<'de>, v: V) -> R<V::Value> {
        match t {
            Text::Borrowed(s) => v.visit_borrowed_str(s),
            Text::Scratch => v.visit_str(&self.scratch),
        }
    }
}

/// The event-level verdict on a frame's `type`.
pub(crate) enum RootTag {
    NotObject,
    Missing,
    NotString,
    Empty,
    Known(&'static str, Obj),
    Unknown,
    /// A repeated unknown discriminator: the `Value` walk decides.
    Reroute,
}

fn fault_of(e: DeErr) -> Fault {
    match e {
        DeErr::Fault(f) => f,
        DeErr::Reroute => Fault::invalid(&Path::ROOT, format_args!("internal: unexpected reroute")),
        DeErr::Custom(m) => Fault::invalid(&Path::ROOT, format_args!("{m}")),
    }
}

/// A value position: the stream, the shape expected here, and where it is.
struct Node<'a, 'de, 'o> {
    st: &'a mut Stream<'de, 'o>,
    shape: Shape,
    path: &'a Path<'a>,
    /// Containers around this value.
    depth: usize,
}

impl<'a, 'de, 'o> Node<'a, 'de, 'o> {
    fn mismatch(&self, expected: &dyn fmt::Display, got: Kind) -> DeErr {
        DeErr::Fault(Fault::mismatch(self.path, expected, got))
    }

    fn invalid(&self, detail: fmt::Arguments<'_>) -> DeErr {
        DeErr::Fault(Fault::invalid(self.path, detail))
    }

    fn expect_string(&mut self, expected: &dyn fmt::Display) -> R<Text<'de>> {
        let kind = self.st.kind()?;
        if kind != Kind::String {
            return Err(self.mismatch(expected, kind));
        }
        self.st.string()
    }

    /// Reads and checks a string position (plain, pointer, literal or
    /// closed set).
    fn checked_string(&mut self) -> R<Text<'de>> {
        match self.shape {
            Shape::Const(c) => {
                let t = self.expect_string(&Quoted(c))?;
                if self.st.text(&t) != c {
                    return Err(self.invalid(format_args!("expected {}", Quoted(c))));
                }
                Ok(Text::Borrowed(c))
            }
            Shape::Enum(set) => {
                let t = self.expect_string(&"string")?;
                match set.iter().find(|m| **m == self.st.text(&t)) {
                    Some(member) => Ok(Text::Borrowed(member)),
                    None if self.st.opts.strict => {
                        Err(self.invalid(format_args!("unrecognised value of a closed set")))
                    }
                    // Optional and list positions were settled by the
                    // lookahead; reaching here means a required position.
                    None => Err(DeErr::Reroute),
                }
            }
            Shape::Pinned { value, set } => {
                let t = self.expect_string(&Quoted(value))?;
                let text = self.st.text(&t);
                if text == value {
                    Ok(Text::Borrowed(value))
                } else if set.contains(&text) || self.st.opts.strict {
                    Err(self.invalid(format_args!("expected {}", Quoted(value))))
                } else {
                    // Outside the set: unrecognised, in a required position
                    // (optional ones were settled by the lookahead).
                    Err(DeErr::Reroute)
                }
            }
            Shape::Restricted { set, domain } => {
                let t = self.expect_string(&"string")?;
                let text = self.st.text(&t);
                match set.iter().find(|m| **m == text) {
                    Some(member) => Ok(Text::Borrowed(member)),
                    None if domain.contains(&text) || self.st.opts.strict => {
                        Err(self.invalid(format_args!("expected {}", OneOf(set))))
                    }
                    // Outside the domain: unrecognised, in a required
                    // position (optional ones were settled by the lookahead).
                    None => Err(DeErr::Reroute),
                }
            }
            Shape::Pointer => {
                let t = self.expect_string(&"string")?;
                if !is_pointer(self.st.text(&t)) {
                    return Err(self.invalid(format_args!("expected a JSON pointer (RFC 6901)")));
                }
                Ok(t)
            }
            _ => self.expect_string(&"string"),
        }
    }

    /// A value whose content is not the schema's business (state, payloads,
    /// metadata values): forwarded faithfully, charged to the budgets.
    fn open<V: Visitor<'de>>(self, v: V) -> R<V::Value> {
        let st = self.st;
        match st.kind()? {
            Kind::Object => {
                st.enter(self.depth + 1, self.path)?;
                st.i += 1;
                let mut access = OpenMap {
                    st,
                    path: self.path,
                    depth: self.depth + 1,
                    first: true,
                    done: false,
                };
                let out = v.visit_map(&mut access)?;
                access.drain()?;
                Ok(out)
            }
            Kind::Array => {
                st.enter(self.depth + 1, self.path)?;
                st.i += 1;
                let mut access = OpenSeq {
                    st,
                    path: self.path,
                    depth: self.depth + 1,
                    first: true,
                    done: false,
                };
                let out = v.visit_seq(&mut access)?;
                access.drain()?;
                Ok(out)
            }
            Kind::String => {
                let t = st.string()?;
                st.visit_text(t, v)
            }
            Kind::Number => match st.number()? {
                Num::U(u) => v.visit_u64(u),
                Num::I(i) => v.visit_i64(i),
                Num::F(f) => v.visit_f64(f),
            },
            Kind::Bool => {
                if st.peek() == Some(b't') {
                    st.literal(b"true")?;
                    v.visit_bool(true)
                } else {
                    st.literal(b"false")?;
                    v.visit_bool(false)
                }
            }
            Kind::Null => {
                st.literal(b"null")?;
                v.visit_unit()
            }
        }
    }

    fn object<V: Visitor<'de>>(self, obj: Obj, v: V) -> R<V::Value> {
        let kind = self.st.kind()?;
        if kind != Kind::Object {
            return Err(self.mismatch(&"object", kind));
        }
        self.st.enter(self.depth + 1, self.path)?;
        self.st.i += 1;
        let mut access = ObjAccess {
            st: self.st,
            obj,
            path: self.path,
            depth: self.depth + 1,
            first: true,
            seen: 0,
            pending: Pending::None,
            done: false,
            hoist: Hoist::from(obj.hoist),
        };
        let out = v.visit_map(&mut access)?;
        access.drain()?;
        Ok(out)
    }

    fn list<V: Visitor<'de>>(self, list: List, v: V) -> R<V::Value> {
        let kind = self.st.kind()?;
        if kind != Kind::Array {
            return Err(self.mismatch(&"array", kind));
        }
        self.st.enter(self.depth + 1, self.path)?;
        self.st.i += 1;
        let mut access = ListAccess {
            st: self.st,
            list,
            path: self.path,
            depth: self.depth + 1,
            first: true,
            index: 0,
            kept: 0,
            done: false,
        };
        let out = v.visit_seq(&mut access)?;
        access.drain()?;
        Ok(out)
    }
}

impl<'a, 'de, 'o> de::Deserializer<'de> for Node<'a, 'de, 'o> {
    type Error = DeErr;

    fn deserialize_any<V: Visitor<'de>>(mut self, v: V) -> R<V::Value> {
        self.st.ws();
        match self.shape {
            Shape::Any => self.open(v),
            Shape::NotNull => {
                if self.st.peek() == Some(b'n') {
                    return Err(self.mismatch(&"a non-null value", Kind::Null));
                }
                self.open(v)
            }
            Shape::OpenObject => {
                let kind = self.st.kind()?;
                if kind != Kind::Object {
                    return Err(self.mismatch(&"object", kind));
                }
                self.open(v)
            }
            Shape::Str
            | Shape::Pointer
            | Shape::Const(_)
            | Shape::Enum(_)
            | Shape::Restricted { .. }
            | Shape::Pinned { .. } => {
                let t = self.checked_string()?;
                self.st.visit_text(t, v)
            }
            Shape::Bool => match self.st.kind()? {
                Kind::Bool => {
                    let value = self.st.peek() == Some(b't');
                    self.st.literal(if value { b"true" } else { b"false" })?;
                    v.visit_bool(value)
                }
                other => Err(self.mismatch(&"boolean", other)),
            },
            Shape::Int { min, max } => {
                let kind = self.st.kind()?;
                if kind != Kind::Number {
                    return Err(self.mismatch(&"integer", kind));
                }
                let n = self.st.number()?;
                match check_int(n, min, max) {
                    Ok(IntVal::U(u)) => v.visit_u64(u),
                    Ok(IntVal::I(i)) => v.visit_i64(i),
                    Err(why) => Err(self.invalid(format_args!("{why} (allowed {min}..={max})"))),
                }
            }
            Shape::Object(obj) => self.object(obj, v),
            Shape::List(list) => self.list(list, v),
            Shape::TextOrList(list) => match self.st.kind()? {
                Kind::String => {
                    let t = self.st.string()?;
                    self.st.visit_text(t, v)
                }
                Kind::Array => self.list(list, v),
                other => Err(self.mismatch(&"string or array", other)),
            },
            Shape::Union(u) => match self.st.scan_tag(u, self.depth, false)? {
                Tag::NotObject(kind) => Err(self.mismatch(&"object", kind)),
                Tag::Missing => {
                    Err(self.invalid(format_args!("missing discriminator `{}`", u.tag)))
                }
                Tag::NotString(kind) => Err(DeErr::Fault(Fault::mismatch(
                    &self.path.key(u.tag),
                    &"string",
                    kind,
                ))),
                Tag::Known(_, obj) => self.object(obj, v),
                Tag::Unknown(_) if self.st.opts.strict => Err(DeErr::Fault(Fault::invalid(
                    &self.path.key(u.tag),
                    format_args!("unrecognised `{}` of {}", u.tag, u.anchor),
                ))),
                Tag::Unknown(_) => Err(DeErr::Reroute),
            },
        }
    }

    fn deserialize_option<V: Visitor<'de>>(self, v: V) -> R<V::Value> {
        self.st.ws();
        if self.st.peek() == Some(b'n') && self.shape.accepts_null() {
            self.st.literal(b"null")?;
            return v.visit_none();
        }
        if self.st.peek() == Some(b'n') {
            // Not acceptable here: report it as the value walk would.
            return self.deserialize_any(v);
        }
        v.visit_some(self)
    }

    fn deserialize_newtype_struct<V: Visitor<'de>>(self, _name: &'static str, v: V) -> R<V::Value> {
        v.visit_newtype_struct(self)
    }

    fn deserialize_enum<V: Visitor<'de>>(
        mut self,
        _name: &'static str,
        _variants: &'static [&'static str],
        v: V,
    ) -> R<V::Value> {
        self.st.ws();
        match self.shape {
            Shape::Str
            | Shape::Pointer
            | Shape::Const(_)
            | Shape::Enum(_)
            | Shape::Restricted { .. }
            | Shape::Pinned { .. } => match self.checked_string()? {
                Text::Borrowed(s) => v.visit_enum(BorrowedStrDeserializer::<DeErr>::new(s)),
                Text::Scratch => v.visit_enum(StrDeserializer::<DeErr>::new(&self.st.scratch)),
            },
            _ => self.deserialize_any(v),
        }
    }

    fn deserialize_ignored_any<V: Visitor<'de>>(self, v: V) -> R<V::Value> {
        // Even an ignored value is enforced: it is still part of the event.
        self.deserialize_any(v)
    }

    forward_to_deserialize_any! {
        bool i8 i16 i32 i64 i128 u8 u16 u32 u64 u128 f32 f64 char str string
        bytes byte_buf unit unit_struct seq tuple tuple_struct map struct
        identifier
    }
}

/// What the next value of a protocol object is.
enum Pending {
    None,
    Field(Field, Option<Obj>),
    /// A member an open object (a JSON Patch operation) does not describe.
    Extra,
    /// The hoisted discriminator's value (already checked by the lookahead).
    Tag(&'static str),
}

/// A union variant's discriminator that is presented ahead of the members
/// preceding it in the text ([`Obj::hoist`]).
#[derive(Clone, Copy)]
enum Hoist {
    /// Nothing to reorder: the discriminator comes first, or this is not a
    /// union variant.
    None,
    /// `(tag, value)` is still to be presented, before any member.
    Due(&'static str, &'static str),
    /// Presented; its occurrence in the text is skipped when reached.
    Done(&'static str),
}

impl From<Option<(&'static str, &'static str)>> for Hoist {
    fn from(hoist: Option<(&'static str, &'static str)>) -> Self {
        match hoist {
            Some((tag, value)) => Hoist::Due(tag, value),
            None => Hoist::None,
        }
    }
}

/// The members of a protocol object.
struct ObjAccess<'a, 'de, 'o> {
    st: &'a mut Stream<'de, 'o>,
    obj: Obj,
    path: &'a Path<'a>,
    /// This object's own nesting level.
    depth: usize,
    first: bool,
    seen: u64,
    pending: Pending,
    done: bool,
    hoist: Hoist,
}

impl<'a, 'de, 'o> ObjAccess<'a, 'de, 'o> {
    /// Steps to the next member's key, or finishes the object.
    fn next_member(&mut self) -> R<bool> {
        if self.done {
            return Ok(false);
        }
        self.st.ws();
        let ended = if self.first {
            self.first = false;
            if self.st.peek() == Some(b'}') {
                self.st.i += 1;
                true
            } else {
                false
            }
        } else {
            self.st.after_item(b'}')?
        };
        if ended {
            self.done = true;
            for (i, field) in self.obj.fields().enumerate() {
                if field.required && self.seen & (1u64 << i) == 0 {
                    return Err(DeErr::Fault(Fault::invalid(
                        self.path,
                        format_args!("missing required field `{}`", field.name),
                    )));
                }
            }
        }
        Ok(!ended)
    }

    fn drain(&mut self) -> R<()> {
        while let Some((IgnoredAny, IgnoredAny)) = self.next_entry::<IgnoredAny, IgnoredAny>()? {}
        Ok(())
    }
}

impl<'a, 'de, 'o> MapAccess<'de> for ObjAccess<'a, 'de, 'o> {
    type Error = DeErr;

    fn next_key_seed<K: DeserializeSeed<'de>>(&mut self, seed: K) -> R<Option<K::Value>> {
        if let Hoist::Due(tag, value) = self.hoist {
            // The lookahead resolved the variant from this member's value, so
            // it is a known string: hand it over first, and skip it in place.
            if let Some((index, _)) = self.obj.find(tag) {
                self.hoist = Hoist::Done(tag);
                self.seen |= 1u64 << index;
                self.pending = Pending::Tag(value);
                return seed
                    .deserialize(BorrowedStrDeserializer::<DeErr>::new(tag))
                    .map(Some);
            }
            self.hoist = Hoist::None;
        }
        loop {
            if !self.next_member()? {
                return Ok(None);
            }
            self.st.add_entry(self.path)?;
            let key = self.st.key()?;
            self.st.ws();
            self.st.expect(b':', "expected `:`")?;
            self.st.ws();
            if let Hoist::Done(tag) = self.hoist {
                if self.st.key_is(&key, tag) {
                    // The in-place occurrence of the tag the lookahead already
                    // validated and presented; a later one hits the seen bit.
                    self.hoist = Hoist::None;
                    self.st.skip_value(self.path, self.depth, false)?;
                    continue;
                }
            }
            let found = match key {
                Key::Plain(bytes, _) => self.obj.find_bytes(bytes),
                Key::Text(t) => self.obj.find(self.st.text(&t)),
            };
            let Some((index, field)) = found else {
                let key = self.st.key_text(key)?;
                if !self.obj.closed {
                    self.pending = Pending::Extra;
                    return match key {
                        Text::Borrowed(s) => seed
                            .deserialize(BorrowedStrDeserializer::<DeErr>::new(s))
                            .map(Some),
                        Text::Scratch => seed
                            .deserialize(StrDeserializer::<DeErr>::new(&self.st.scratch))
                            .map(Some),
                    };
                }
                let anchor = self.obj.anchor;
                if self.st.opts.strict {
                    let here = self.path.key(self.st.text(&key));
                    return Err(DeErr::Fault(Fault::invalid(
                        &here,
                        format_args!("unrecognised property of {anchor}"),
                    )));
                }
                if self.st.findings.has_room() {
                    let pointer = self.path.key(self.st.text(&key)).pointer();
                    self.st.findings.push_owned(
                        DiagnosticCode::UnknownField,
                        pointer,
                        format!("unrecognised property of {anchor} removed"),
                    );
                } else {
                    self.st.findings.suppressed += 1;
                }
                self.st.skip_value(self.path, self.depth, true)?;
                continue;
            };
            if self.seen & (1u64 << index) != 0 {
                if !self.st.opts.strict {
                    // Last occurrence wins, as `JSON.parse` has it: the
                    // value walk resolves it.
                    return Err(DeErr::Reroute);
                }
                return Err(DeErr::Fault(Fault::invalid(
                    self.path,
                    format_args!("duplicate member `{}`", field.name),
                )));
            }
            self.seen |= 1u64 << index;
            let here = self.path.key(field.name);
            if self.st.peek() == Some(b'n') && !field.shape.accepts_null() {
                if field.required
                    || !self.st.opts.null_as_absent
                    || self.st.opts.strict
                    || !self.obj.null_reads_absent(field.name)
                {
                    return Err(DeErr::Fault(Fault::mismatch(
                        &here,
                        &expected_name(field.shape),
                        Kind::Null,
                    )));
                }
                self.st.literal(b"null")?;
                self.st
                    .findings
                    .push(DiagnosticCode::OptionalNull, &here, || {
                        "`null` in an optional position read as absent".to_owned()
                    });
                continue;
            }
            match self.st.precheck(field.shape, self.depth)? {
                Pre::Unknown(_) if field.required => return Err(DeErr::Reroute),
                Pre::Unknown(removal) => {
                    self.st
                        .findings
                        .push(removal.code, &here, || removal.message());
                    self.st.skip_value(&here, self.depth, true)?;
                    continue;
                }
                Pre::Go(hint) => {
                    self.pending = Pending::Field(field, hint);
                    return seed
                        .deserialize(BorrowedStrDeserializer::<DeErr>::new(field.name))
                        .map(Some);
                }
            }
        }
    }

    fn next_value_seed<V: DeserializeSeed<'de>>(&mut self, seed: V) -> R<V::Value> {
        match std::mem::replace(&mut self.pending, Pending::None) {
            Pending::Field(field, hint) => {
                let here = self.path.key(field.name);
                seed.deserialize(Node {
                    st: self.st,
                    shape: hint.map_or(field.shape, Shape::Object),
                    path: &here,
                    depth: self.depth,
                })
            }
            Pending::Extra => seed.deserialize(Node {
                st: self.st,
                shape: Shape::Any,
                path: self.path,
                depth: self.depth,
            }),
            Pending::Tag(value) => seed.deserialize(BorrowedStrDeserializer::<DeErr>::new(value)),
            Pending::None => Err(DeErr::Custom("value requested before its key".to_owned())),
        }
    }
}

fn expected_name(shape: Shape) -> &'static str {
    match shape {
        Shape::Str
        | Shape::Pointer
        | Shape::Enum(_)
        | Shape::Restricted { .. }
        | Shape::Const(_)
        | Shape::Pinned { .. } => "string",
        Shape::Bool => "boolean",
        Shape::Int { .. } => "integer",
        Shape::Object(_) | Shape::OpenObject | Shape::Union(_) => "object",
        Shape::List(_) => "array",
        Shape::TextOrList(_) => "string or array",
        Shape::Any | Shape::NotNull => "a non-null value",
    }
}

/// The elements of a protocol array.
struct ListAccess<'a, 'de, 'o> {
    st: &'a mut Stream<'de, 'o>,
    list: List,
    path: &'a Path<'a>,
    depth: usize,
    first: bool,
    /// Elements read so far, kept or not (the original index).
    index: usize,
    kept: usize,
    done: bool,
}

impl<'a, 'de, 'o> ListAccess<'a, 'de, 'o> {
    fn drain(&mut self) -> R<()> {
        while let Some(IgnoredAny) = self.next_element::<IgnoredAny>()? {}
        Ok(())
    }
}

impl<'a, 'de, 'o> SeqAccess<'de> for ListAccess<'a, 'de, 'o> {
    type Error = DeErr;

    fn next_element_seed<T: DeserializeSeed<'de>>(&mut self, seed: T) -> R<Option<T::Value>> {
        loop {
            if self.done {
                return Ok(None);
            }
            self.st.ws();
            let ended = if self.first {
                self.first = false;
                if self.st.peek() == Some(b']') {
                    self.st.i += 1;
                    true
                } else {
                    false
                }
            } else {
                self.st.after_item(b']')?
            };
            if ended {
                self.done = true;
                if self.index < self.list.min_items {
                    return Err(DeErr::Fault(Fault::invalid(
                        self.path,
                        format_args!("expected at least {} element(s)", self.list.min_items),
                    )));
                }
                if self.kept < self.list.min_items {
                    // Dropping made the list too short: its holder decides.
                    return Err(DeErr::Reroute);
                }
                return Ok(None);
            }
            self.st.add_entry(self.path)?;
            let index = self.index;
            self.index += 1;
            let here = self.path.index(index);
            match self.st.precheck(self.list.items(), self.depth)? {
                Pre::Unknown(removal) => {
                    self.st
                        .findings
                        .push(removal.code, &here, || removal.message());
                    self.st.skip_value(&here, self.depth, true)?;
                }
                Pre::Go(hint) => {
                    self.kept += 1;
                    return seed
                        .deserialize(Node {
                            st: self.st,
                            shape: hint.map_or(self.list.items(), Shape::Object),
                            path: &here,
                            depth: self.depth,
                        })
                        .map(Some);
                }
            }
        }
    }
}

/// Members of an open object.
struct OpenMap<'a, 'de, 'o> {
    st: &'a mut Stream<'de, 'o>,
    path: &'a Path<'a>,
    depth: usize,
    first: bool,
    done: bool,
}

impl<'a, 'de, 'o> OpenMap<'a, 'de, 'o> {
    fn drain(&mut self) -> R<()> {
        while let Some((IgnoredAny, IgnoredAny)) = self.next_entry::<IgnoredAny, IgnoredAny>()? {}
        Ok(())
    }
}

impl<'a, 'de, 'o> MapAccess<'de> for OpenMap<'a, 'de, 'o> {
    type Error = DeErr;

    fn next_key_seed<K: DeserializeSeed<'de>>(&mut self, seed: K) -> R<Option<K::Value>> {
        if self.done {
            return Ok(None);
        }
        self.st.ws();
        let ended = if self.first {
            self.first = false;
            if self.st.peek() == Some(b'}') {
                self.st.i += 1;
                true
            } else {
                false
            }
        } else {
            self.st.after_item(b'}')?
        };
        if ended {
            self.done = true;
            return Ok(None);
        }
        self.st.add_entry(self.path)?;
        let key = self.st.string()?;
        self.st.ws();
        self.st.expect(b':', "expected `:`")?;
        self.st.ws();
        match key {
            Text::Borrowed(s) => seed.deserialize(BorrowedStrDeserializer::<DeErr>::new(s)),
            Text::Scratch => seed.deserialize(StrDeserializer::<DeErr>::new(&self.st.scratch)),
        }
        .map(Some)
    }

    fn next_value_seed<V: DeserializeSeed<'de>>(&mut self, seed: V) -> R<V::Value> {
        seed.deserialize(Node {
            st: self.st,
            shape: Shape::Any,
            path: self.path,
            depth: self.depth,
        })
    }
}

/// Elements of an open array.
struct OpenSeq<'a, 'de, 'o> {
    st: &'a mut Stream<'de, 'o>,
    path: &'a Path<'a>,
    depth: usize,
    first: bool,
    done: bool,
}

impl<'a, 'de, 'o> OpenSeq<'a, 'de, 'o> {
    fn drain(&mut self) -> R<()> {
        while let Some(IgnoredAny) = self.next_element::<IgnoredAny>()? {}
        Ok(())
    }
}

impl<'a, 'de, 'o> SeqAccess<'de> for OpenSeq<'a, 'de, 'o> {
    type Error = DeErr;

    fn next_element_seed<T: DeserializeSeed<'de>>(&mut self, seed: T) -> R<Option<T::Value>> {
        if self.done {
            return Ok(None);
        }
        self.st.ws();
        let ended = if self.first {
            self.first = false;
            if self.st.peek() == Some(b']') {
                self.st.i += 1;
                true
            } else {
                false
            }
        } else {
            self.st.after_item(b']')?
        };
        if ended {
            self.done = true;
            return Ok(None);
        }
        self.st.add_entry(self.path)?;
        seed.deserialize(Node {
            st: self.st,
            shape: Shape::Any,
            path: self.path,
            depth: self.depth,
        })
        .map(Some)
    }
}
