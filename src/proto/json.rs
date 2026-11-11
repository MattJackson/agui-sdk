//! A minimal, strict (RFC 8259 / `JSON.parse`) JSON reader that builds a flat,
//! ordered, borrowing tree, plus the [`JNode`] view the encoder walks. The view
//! is implemented both for that tree and for `serde_json::Value`, so the
//! encoder is written once.
//!
//! Strings are never copied: a node records the raw slice between the quotes
//! and whether it contains escapes. Escapes are expanded straight into the
//! protobuf output (or measured in place for the size pass).

use super::codec_error::{Error, Result};
use std::borrow::Cow;

/// Maximum JSON container nesting accepted by the encoder and by the
/// `json::build` builder. The decoder has its own, lower ceiling,
/// `decoder::DECODE_MAX_DEPTH`.
pub const MAX_DEPTH: usize = 512;

/// A JSON string as it appears in the source: either free of escapes, or raw
/// text still containing (already validated) escape sequences.
#[derive(Clone, Copy, Debug)]
pub enum JStr<'a> {
    /// No escapes: the slice is the value.
    Plain(&'a str),
    /// Raw text between the quotes, escapes not yet expanded.
    Escaped(&'a str),
}

impl<'a> JStr<'a> {
    /// Length in bytes of the UTF-8 value.
    #[inline]
    pub fn len(&self) -> usize {
        match *self {
            JStr::Plain(s) => s.len(),
            JStr::Escaped(raw) => {
                let mut n = 0;
                unescape_pieces(raw, |p| n += p.len());
                n
            }
        }
    }

    /// Appends the UTF-8 value.
    #[inline]
    pub fn write_to(&self, out: &mut Vec<u8>) {
        match *self {
            JStr::Plain(s) => out.extend_from_slice(s.as_bytes()),
            JStr::Escaped(raw) => unescape_pieces(raw, |p| out.extend_from_slice(p)),
        }
    }

    /// The value, borrowed when possible.
    pub fn cow(&self) -> Cow<'a, str> {
        match *self {
            JStr::Plain(s) => Cow::Borrowed(s),
            JStr::Escaped(raw) => {
                let mut v = Vec::with_capacity(raw.len());
                unescape_pieces(raw, |p| v.extend_from_slice(p));
                // Pieces are slices of valid UTF-8 or encoded chars.
                Cow::Owned(String::from_utf8(v).unwrap_or_default())
            }
        }
    }

    /// Compares the value with `s`.
    #[inline]
    pub fn eq_str(&self, s: &str) -> bool {
        match *self {
            JStr::Plain(p) => p == s,
            JStr::Escaped(_) => self.cow() == s,
        }
    }
}

/// Expands escapes in validated raw string text, handing out pieces.
/// Lone surrogates become U+FFFD, as a JS `TextEncoder` would.
fn unescape_pieces(raw: &str, mut f: impl FnMut(&[u8])) {
    let b = raw.as_bytes();
    let mut i = 0;
    let mut run = 0;
    while i < b.len() {
        if b[i] != b'\\' {
            i += 1;
            continue;
        }
        if run < i {
            f(&b[run..i]);
        }
        let c = b[i + 1];
        i += 2;
        let ch: char = match c {
            b'"' => '"',
            b'\\' => '\\',
            b'/' => '/',
            b'b' => '\u{8}',
            b'f' => '\u{c}',
            b'n' => '\n',
            b'r' => '\r',
            b't' => '\t',
            _ => {
                // 'u'
                let hi = hex4(&b[i..i + 4]);
                i += 4;
                if (0xD800..0xDC00).contains(&hi)
                    && i + 6 <= b.len()
                    && b[i] == b'\\'
                    && b[i + 1] == b'u'
                {
                    let lo = hex4(&b[i + 2..i + 6]);
                    if (0xDC00..0xE000).contains(&lo) {
                        i += 6;
                        let cp = 0x10000 + ((hi - 0xD800) << 10) + (lo - 0xDC00);
                        char::from_u32(cp).unwrap_or('\u{FFFD}')
                    } else {
                        '\u{FFFD}'
                    }
                } else {
                    char::from_u32(hi).unwrap_or('\u{FFFD}')
                }
            }
        };
        let mut buf = [0u8; 4];
        f(ch.encode_utf8(&mut buf).as_bytes());
        run = i;
    }
    if run < b.len() {
        f(&b[run..]);
    }
}

#[inline]
fn hex_val(c: u8) -> Option<u32> {
    match c {
        b'0'..=b'9' => Some((c - b'0') as u32),
        b'a'..=b'f' => Some((c - b'a' + 10) as u32),
        b'A'..=b'F' => Some((c - b'A' + 10) as u32),
        _ => None,
    }
}

fn hex4(b: &[u8]) -> u32 {
    b.iter()
        .fold(0, |acc, &c| (acc << 4) | hex_val(c).unwrap_or(0))
}

// ---------------------------------------------------------------------------
// The view the encoder walks.

/// JSON value kind.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// `null`
    Null,
    /// `true` / `false`
    Bool,
    /// a number
    Num,
    /// a string
    Str,
    /// an array
    Arr,
    /// an object
    Obj,
}

impl Kind {
    pub(crate) fn name(self) -> &'static str {
        match self {
            Kind::Null => "null",
            Kind::Bool => "boolean",
            Kind::Num => "number",
            Kind::Str => "string",
            Kind::Arr => "array",
            Kind::Obj => "object",
        }
    }
}

/// Read-only access to a JSON node, in source (insertion) order.
pub trait JNode<'a>: Copy {
    /// Whether an object may hold the same key twice (text input can).
    const MAY_HAVE_DUPLICATE_KEYS: bool;
    /// Kind of this node.
    fn kind(self) -> Kind;
    /// Boolean value (kind must be `Bool`).
    fn as_bool(self) -> bool;
    /// Numeric value as a JS number (kind must be `Num`).
    fn as_f64(self) -> f64;
    /// String value (kind must be `Str`).
    fn as_str(self) -> JStr<'a>;
    /// Array elements (kind must be `Arr`).
    fn elements(self) -> impl Iterator<Item = Self>;
    /// Object members in insertion order (kind must be `Obj`).
    fn members(self) -> impl Iterator<Item = (JStr<'a>, Self)>;
    /// Number of object members.
    fn member_count(self) -> usize;
    /// The `i`-th object member.
    fn member(self, i: usize) -> (JStr<'a>, Self);
}

// ---------------------------------------------------------------------------
// Flat tree.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Tag {
    Null,
    True,
    False,
    Num,
    /// A number already converted to an `f64` (from a typed value): the
    /// bits are `a` (low half) and `b` (high half).
    F64,
    Str,
    StrEsc,
    Arr,
    Obj,
}

/// `a`/`b`: string or number byte range in the source; for containers, the
/// start in `kids` and the element (or member) count.
#[derive(Clone, Copy, Debug)]
struct Node {
    tag: Tag,
    a: u32,
    b: u32,
}

/// Reusable parse buffers.
#[derive(Default, Debug)]
pub struct Scratch {
    nodes: Vec<Node>,
    kids: Vec<u32>,
    stack: Vec<u32>,
}

impl Scratch {
    /// Empties the buffers, keeping their capacity.
    pub(super) fn clear(&mut self) {
        self.nodes.clear();
        self.kids.clear();
        self.stack.clear();
    }

    /// Releases capacity beyond `keep` entries per buffer.
    pub(super) fn shrink_to(&mut self, keep: usize) {
        self.nodes.shrink_to(keep);
        self.kids.shrink_to(keep);
        self.stack.shrink_to(keep);
    }

    /// The capacity held, in bytes.
    pub(super) fn capacity_bytes(&self) -> usize {
        self.nodes.capacity() * std::mem::size_of::<Node>()
            + (self.kids.capacity() + self.stack.capacity()) * std::mem::size_of::<u32>()
    }

    /// The largest buffer capacity, in entries.
    #[cfg(test)]
    pub(super) fn max_capacity(&self) -> usize {
        self.nodes
            .capacity()
            .max(self.kids.capacity())
            .max(self.stack.capacity())
    }
}

/// A parsed document borrowing its source and the scratch buffers.
#[derive(Debug)]
pub struct Tree<'a> {
    src: &'a str,
    nodes: &'a [Node],
    kids: &'a [u32],
}

/// A node of a [`Tree`].
#[derive(Clone, Copy, Debug)]
pub struct TNode<'a> {
    t: &'a Tree<'a>,
    i: u32,
}

impl<'a> Tree<'a> {
    /// The root node.
    pub fn root(&'a self) -> TNode<'a> {
        TNode { t: self, i: 0 }
    }
}

impl<'a> TNode<'a> {
    #[inline]
    fn node(self) -> Node {
        self.t.nodes[self.i as usize]
    }
    #[inline]
    fn at(self, i: u32) -> TNode<'a> {
        TNode { t: self.t, i }
    }
    #[inline]
    fn text(self, n: Node) -> &'a str {
        &self.t.src[n.a as usize..n.b as usize]
    }
}

impl<'a> JNode<'a> for TNode<'a> {
    const MAY_HAVE_DUPLICATE_KEYS: bool = true;

    #[inline]
    fn kind(self) -> Kind {
        match self.node().tag {
            Tag::Null => Kind::Null,
            Tag::True | Tag::False => Kind::Bool,
            Tag::Num | Tag::F64 => Kind::Num,
            Tag::Str | Tag::StrEsc => Kind::Str,
            Tag::Arr => Kind::Arr,
            Tag::Obj => Kind::Obj,
        }
    }
    #[inline]
    fn as_bool(self) -> bool {
        self.node().tag == Tag::True
    }
    #[inline]
    fn as_f64(self) -> f64 {
        let n = self.node();
        if n.tag == Tag::F64 {
            return f64::from_bits(u64::from(n.a) | (u64::from(n.b) << 32));
        }
        // Validated JSON number grammar is a subset of Rust's float syntax,
        // and the conversion is correctly rounded, as `JSON.parse` is.
        self.text(n).parse().unwrap_or(f64::NAN)
    }
    #[inline]
    fn as_str(self) -> JStr<'a> {
        let n = self.node();
        let s = self.text(n);
        if n.tag == Tag::StrEsc {
            JStr::Escaped(s)
        } else {
            JStr::Plain(s)
        }
    }
    fn elements(self) -> impl Iterator<Item = Self> {
        let n = self.node();
        let ks = &self.t.kids[n.a as usize..(n.a + n.b) as usize];
        ks.iter().map(move |&k| self.at(k))
    }
    fn members(self) -> impl Iterator<Item = (JStr<'a>, Self)> {
        let n = self.node();
        let ks = &self.t.kids[n.a as usize..(n.a + 2 * n.b) as usize];
        ks.chunks_exact(2)
            .map(move |p| (self.at(p[0]).as_str(), self.at(p[1])))
    }
    #[inline]
    fn member_count(self) -> usize {
        self.node().b as usize
    }
    #[inline]
    fn member(self, i: usize) -> (JStr<'a>, Self) {
        let n = self.node();
        let k = n.a as usize + 2 * i;
        (
            self.at(self.t.kids[k]).as_str(),
            self.at(self.t.kids[k + 1]),
        )
    }
}

impl<'a> JNode<'a> for &'a serde_json::Value {
    const MAY_HAVE_DUPLICATE_KEYS: bool = false;

    #[inline]
    fn kind(self) -> Kind {
        use serde_json::Value as V;
        match self {
            V::Null => Kind::Null,
            V::Bool(_) => Kind::Bool,
            V::Number(_) => Kind::Num,
            V::String(_) => Kind::Str,
            V::Array(_) => Kind::Arr,
            V::Object(_) => Kind::Obj,
        }
    }
    fn as_bool(self) -> bool {
        serde_json::Value::as_bool(self).unwrap_or(false)
    }
    fn as_f64(self) -> f64 {
        serde_json::Value::as_f64(self).unwrap_or(f64::NAN)
    }
    fn as_str(self) -> JStr<'a> {
        JStr::Plain(serde_json::Value::as_str(self).unwrap_or(""))
    }
    fn elements(self) -> impl Iterator<Item = Self> {
        serde_json::Value::as_array(self)
            .map(|a| a.iter())
            .into_iter()
            .flatten()
    }
    fn members(self) -> impl Iterator<Item = (JStr<'a>, Self)> {
        serde_json::Value::as_object(self)
            .map(|o| o.iter())
            .into_iter()
            .flatten()
            .map(|(k, v)| (JStr::Plain(k.as_str()), v))
    }
    fn member_count(self) -> usize {
        serde_json::Value::as_object(self).map_or(0, |o| o.len())
    }
    fn member(self, i: usize) -> (JStr<'a>, Self) {
        // Positional access is O(i) on a map; the encoder only uses it for
        // input that may repeat a key, which a map cannot. An index out of
        // range (never asked for) yields an empty key and this node.
        serde_json::Value::as_object(self)
            .and_then(|o| o.iter().nth(i))
            .map_or((JStr::Plain(""), self), |(k, v)| {
                (JStr::Plain(k.as_str()), v)
            })
    }
}

// ---------------------------------------------------------------------------
// Parser.

/// Parses `src` into `scratch`, returning a tree view. Strict JSON: one value,
/// optional surrounding whitespace, no trailing commas, no comments.
pub fn parse<'a>(src: &'a str, scratch: &'a mut Scratch) -> Result<Tree<'a>> {
    scratch.nodes.clear();
    scratch.kids.clear();
    scratch.stack.clear();
    if src.len() > u32::MAX as usize {
        return Err(Error::TooLarge);
    }
    let mut p = Parser {
        b: src.as_bytes(),
        i: 0,
        s: scratch,
    };
    p.ws();
    p.value(0)?;
    p.ws();
    if p.i != p.b.len() {
        return Err(p.err("trailing characters after the JSON value"));
    }
    Ok(Tree {
        src,
        nodes: &scratch.nodes,
        kids: &scratch.kids,
    })
}

struct Parser<'b, 's> {
    b: &'b [u8],
    i: usize,
    s: &'s mut Scratch,
}

impl Parser<'_, '_> {
    fn err(&self, reason: &'static str) -> Error {
        Error::Json {
            offset: self.i,
            reason,
        }
    }

    #[inline]
    fn ws(&mut self) {
        while let Some(&c) = self.b.get(self.i) {
            if c == b' ' || c == b'\n' || c == b'\r' || c == b'\t' {
                self.i += 1;
            } else {
                break;
            }
        }
    }

    #[inline]
    fn push(&mut self, tag: Tag, a: usize, b: usize) -> u32 {
        let idx = self.s.nodes.len() as u32;
        self.s.nodes.push(Node {
            tag,
            a: a as u32,
            b: b as u32,
        });
        idx
    }

    fn value(&mut self, depth: usize) -> Result<u32> {
        match self.b.get(self.i) {
            None => Err(self.err("unexpected end of input")),
            Some(b'{') => self.object(depth + 1),
            Some(b'[') => self.array(depth + 1),
            Some(b'"') => self.string(),
            Some(b't') => self.lit(b"true", Tag::True),
            Some(b'f') => self.lit(b"false", Tag::False),
            Some(b'n') => self.lit(b"null", Tag::Null),
            Some(b'-' | b'0'..=b'9') => self.number(),
            Some(_) => Err(self.err("unexpected character")),
        }
    }

    fn lit(&mut self, word: &'static [u8], tag: Tag) -> Result<u32> {
        if self.b[self.i..].starts_with(word) {
            self.i += word.len();
            Ok(self.push(tag, 0, 0))
        } else {
            Err(self.err("invalid literal"))
        }
    }

    fn digits(&mut self) -> usize {
        let start = self.i;
        while matches!(self.b.get(self.i), Some(b'0'..=b'9')) {
            self.i += 1;
        }
        self.i - start
    }

    fn number(&mut self) -> Result<u32> {
        let start = self.i;
        if self.b[self.i] == b'-' {
            self.i += 1;
        }
        match self.b.get(self.i) {
            Some(b'0') => self.i += 1,
            Some(b'1'..=b'9') => {
                self.digits();
            }
            _ => return Err(self.err("invalid number")),
        }
        if self.b.get(self.i) == Some(&b'.') {
            self.i += 1;
            if self.digits() == 0 {
                return Err(self.err("invalid number"));
            }
        }
        if matches!(self.b.get(self.i), Some(b'e' | b'E')) {
            self.i += 1;
            if matches!(self.b.get(self.i), Some(b'+' | b'-')) {
                self.i += 1;
            }
            if self.digits() == 0 {
                return Err(self.err("invalid number"));
            }
        }
        Ok(self.push(Tag::Num, start, self.i))
    }

    fn string(&mut self) -> Result<u32> {
        self.i += 1; // opening quote
        let start = self.i;
        let mut escaped = false;
        let b = self.b;
        loop {
            // Word-at-a-time skip over bytes that need no attention.
            while self.i + 8 <= b.len() {
                let mut w = [0u8; 8];
                w.copy_from_slice(&b[self.i..self.i + 8]);
                let v = u64::from_le_bytes(w);
                if needs_attention(v) {
                    break;
                }
                self.i += 8;
            }
            match b.get(self.i) {
                None => return Err(self.err("unterminated string")),
                Some(b'"') => {
                    let end = self.i;
                    self.i += 1;
                    let tag = if escaped { Tag::StrEsc } else { Tag::Str };
                    return Ok(self.push(tag, start, end));
                }
                Some(b'\\') => {
                    escaped = true;
                    match b.get(self.i + 1) {
                        Some(b'"' | b'\\' | b'/' | b'b' | b'f' | b'n' | b'r' | b't') => self.i += 2,
                        Some(b'u') => {
                            let ok = b.len() >= self.i + 6
                                && b[self.i + 2..self.i + 6]
                                    .iter()
                                    .all(|&c| hex_val(c).is_some());
                            if !ok {
                                return Err(self.err("invalid \\u escape"));
                            }
                            self.i += 6;
                        }
                        _ => return Err(self.err("invalid escape")),
                    }
                }
                Some(&c) if c < 0x20 => return Err(self.err("control character in string")),
                Some(_) => self.i += 1,
            }
        }
    }

    fn array(&mut self, depth: usize) -> Result<u32> {
        if depth > MAX_DEPTH {
            return Err(Error::DepthLimit);
        }
        self.i += 1;
        let me = self.push(Tag::Arr, 0, 0);
        let base = self.s.stack.len();
        self.ws();
        if self.b.get(self.i) == Some(&b']') {
            self.i += 1;
        } else {
            loop {
                self.ws();
                let v = self.value(depth)?;
                self.s.stack.push(v);
                self.ws();
                match self.b.get(self.i) {
                    Some(b',') => self.i += 1,
                    Some(b']') => {
                        self.i += 1;
                        break;
                    }
                    _ => return Err(self.err("expected ',' or ']'")),
                }
            }
        }
        let a = self.s.kids.len();
        let n = self.s.stack.len() - base;
        self.s.kids.extend(self.s.stack.drain(base..));
        self.s.nodes[me as usize].a = a as u32;
        self.s.nodes[me as usize].b = n as u32;
        Ok(me)
    }

    fn object(&mut self, depth: usize) -> Result<u32> {
        if depth > MAX_DEPTH {
            return Err(Error::DepthLimit);
        }
        self.i += 1;
        let me = self.push(Tag::Obj, 0, 0);
        let base = self.s.stack.len();
        self.ws();
        if self.b.get(self.i) == Some(&b'}') {
            self.i += 1;
        } else {
            loop {
                self.ws();
                if self.b.get(self.i) != Some(&b'"') {
                    return Err(self.err("expected object key"));
                }
                let k = self.string()?;
                self.ws();
                if self.b.get(self.i) != Some(&b':') {
                    return Err(self.err("expected ':'"));
                }
                self.i += 1;
                self.ws();
                let v = self.value(depth)?;
                self.s.stack.push(k);
                self.s.stack.push(v);
                self.ws();
                match self.b.get(self.i) {
                    Some(b',') => self.i += 1,
                    Some(b'}') => {
                        self.i += 1;
                        break;
                    }
                    _ => return Err(self.err("expected ',' or '}'")),
                }
            }
        }
        let a = self.s.kids.len();
        let n = (self.s.stack.len() - base) / 2;
        self.s.kids.extend(self.s.stack.drain(base..));
        self.s.nodes[me as usize].a = a as u32;
        self.s.nodes[me as usize].b = n as u32;
        Ok(me)
    }
}

// ---------------------------------------------------------------------------
// Typed builder: the same tree, straight from a `Serialize` value.

/// Builds the tree [`parse`] would make from `serde_json::to_vec(value)`,
/// without writing or reading that text: strings are copied unescaped into
/// `text` (so every string node is plain), numbers are kept as the `f64`
/// `JSON.parse` would read back, and members keep serialization order.
///
/// Returns `None` for anything this builder does not mirror exactly
/// (`serde_json`'s private number and raw-value types, `f32`, byte strings,
/// map keys other than strings, integers and booleans, nesting deeper than
/// [`MAX_DEPTH`]); the caller then takes the JSON text path, so the output
/// never depends on which path ran.
pub fn build<'a, T: serde::Serialize + ?Sized>(
    value: &T,
    text: &'a mut String,
    scratch: &'a mut Scratch,
) -> Option<Tree<'a>> {
    scratch.nodes.clear();
    scratch.kids.clear();
    scratch.stack.clear();
    text.clear();
    let mut b = Builder {
        text,
        s: scratch,
        depth: 0,
    };
    let root = value.serialize(&mut b).ok()?;
    let Builder { text, s, .. } = b;
    if root != 0 || text.len() > u32::MAX as usize {
        return None;
    }
    Some(Tree {
        src: text,
        nodes: &s.nodes,
        kids: &s.kids,
    })
}

struct Builder<'s> {
    text: &'s mut String,
    s: &'s mut Scratch,
    depth: usize,
}

/// The builder declined; the caller falls back to the text path.
#[derive(Debug)]
pub(super) struct Decline;

impl std::fmt::Display for Decline {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("not representable by the typed builder")
    }
}

impl std::error::Error for Decline {}

impl serde::ser::Error for Decline {
    fn custom<T: std::fmt::Display>(_: T) -> Self {
        Decline
    }
}

type Built = std::result::Result<u32, Decline>;
type Step = std::result::Result<(), Decline>;

impl Builder<'_> {
    fn push(&mut self, tag: Tag, a: u32, b: u32) -> Built {
        let idx = u32::try_from(self.s.nodes.len()).map_err(|_| Decline)?;
        self.s.nodes.push(Node { tag, a, b });
        Ok(idx)
    }

    fn number(&mut self, x: f64) -> Built {
        // `serde_json` writes a non-finite float as `null`.
        if !x.is_finite() {
            return self.push(Tag::Null, 0, 0);
        }
        let bits = x.to_bits();
        self.push(Tag::F64, bits as u32, (bits >> 32) as u32)
    }

    fn offset(&self) -> std::result::Result<u32, Decline> {
        u32::try_from(self.text.len()).map_err(|_| Decline)
    }

    fn string(&mut self, v: &str) -> Built {
        let a = self.offset()?;
        self.text.push_str(v);
        let b = self.offset()?;
        self.push(Tag::Str, a, b)
    }

    /// Opens a container: its node, and where its children start on the stack.
    fn open(&mut self, tag: Tag) -> std::result::Result<(u32, usize), Decline> {
        self.depth += 1;
        if self.depth > MAX_DEPTH {
            return Err(Decline);
        }
        let me = self.push(tag, 0, 0)?;
        Ok((me, self.s.stack.len()))
    }

    fn close(&mut self, (me, base): (u32, usize), per: usize) -> Built {
        self.depth -= 1;
        let a = u32::try_from(self.s.kids.len()).map_err(|_| Decline)?;
        let n = u32::try_from((self.s.stack.len() - base) / per).map_err(|_| Decline)?;
        self.s.kids.extend(self.s.stack.drain(base..));
        let node = &mut self.s.nodes[me as usize];
        node.a = a;
        node.b = n;
        Ok(me)
    }

    /// `{"variant": <container>}`, for the externally tagged forms.
    fn open_variant(
        &mut self,
        variant: &'static str,
        tag: Tag,
    ) -> std::result::Result<Open, Decline> {
        let outer = self.open(Tag::Obj)?;
        let key = self.string(variant)?;
        self.s.stack.push(key);
        let inner = self.open(tag)?;
        Ok(Open {
            outer: Some(outer),
            inner,
        })
    }
}

/// An open container, possibly inside a one-member variant object.
pub(super) struct Open {
    outer: Option<(u32, usize)>,
    inner: (u32, usize),
}

/// Writes a container's entries.
pub(super) struct Container<'b, 's> {
    b: &'b mut Builder<'s>,
    open: Open,
    /// Stack slots per entry: 1 for arrays, 2 for objects.
    per: usize,
}

impl Container<'_, '_> {
    fn element<T: serde::Serialize + ?Sized>(&mut self, value: &T) -> Step {
        let v = value.serialize(&mut *self.b)?;
        self.b.s.stack.push(v);
        Ok(())
    }

    fn key<T: serde::Serialize + ?Sized>(&mut self, key: &T) -> Step {
        let k = key.serialize(KeyBuilder(&mut *self.b))?;
        self.b.s.stack.push(k);
        Ok(())
    }

    fn field<T: serde::Serialize + ?Sized>(&mut self, key: &'static str, value: &T) -> Step {
        let k = self.b.string(key)?;
        self.b.s.stack.push(k);
        self.element(value)
    }

    fn finish(self) -> Built {
        let inner = self.b.close(self.open.inner, self.per)?;
        match self.open.outer {
            None => Ok(inner),
            Some(outer) => {
                self.b.s.stack.push(inner);
                self.b.close(outer, 2)
            }
        }
    }
}

impl<'b, 's> serde::Serializer for &'b mut Builder<'s> {
    type Ok = u32;
    type Error = Decline;
    type SerializeSeq = Container<'b, 's>;
    type SerializeTuple = Container<'b, 's>;
    type SerializeTupleStruct = Container<'b, 's>;
    type SerializeTupleVariant = Container<'b, 's>;
    type SerializeMap = Container<'b, 's>;
    type SerializeStruct = Container<'b, 's>;
    type SerializeStructVariant = Container<'b, 's>;

    fn serialize_bool(self, v: bool) -> Built {
        self.push(if v { Tag::True } else { Tag::False }, 0, 0)
    }
    // Integers become the nearest `f64`, which is what `JSON.parse` makes of
    // their decimal text (`as` rounds to nearest, ties to even).
    fn serialize_i8(self, v: i8) -> Built {
        self.number(f64::from(v))
    }
    fn serialize_i16(self, v: i16) -> Built {
        self.number(f64::from(v))
    }
    fn serialize_i32(self, v: i32) -> Built {
        self.number(f64::from(v))
    }
    fn serialize_i64(self, v: i64) -> Built {
        self.number(v as f64)
    }
    fn serialize_i128(self, v: i128) -> Built {
        self.number(v as f64)
    }
    fn serialize_u8(self, v: u8) -> Built {
        self.number(f64::from(v))
    }
    fn serialize_u16(self, v: u16) -> Built {
        self.number(f64::from(v))
    }
    fn serialize_u32(self, v: u32) -> Built {
        self.number(f64::from(v))
    }
    fn serialize_u64(self, v: u64) -> Built {
        self.number(v as f64)
    }
    fn serialize_u128(self, v: u128) -> Built {
        self.number(v as f64)
    }
    fn serialize_f32(self, _: f32) -> Built {
        // Written as the shortest `f32` digits, then read back as an `f64`:
        // not mirrored (no protocol type has an `f32`).
        Err(Decline)
    }
    fn serialize_f64(self, v: f64) -> Built {
        self.number(v)
    }
    fn serialize_char(self, v: char) -> Built {
        self.string(v.encode_utf8(&mut [0; 4]))
    }
    fn serialize_str(self, v: &str) -> Built {
        self.string(v)
    }
    fn serialize_bytes(self, _: &[u8]) -> Built {
        Err(Decline)
    }
    fn serialize_none(self) -> Built {
        self.push(Tag::Null, 0, 0)
    }
    fn serialize_some<T: serde::Serialize + ?Sized>(self, value: &T) -> Built {
        value.serialize(self)
    }
    fn serialize_unit(self) -> Built {
        self.push(Tag::Null, 0, 0)
    }
    fn serialize_unit_struct(self, _: &'static str) -> Built {
        self.push(Tag::Null, 0, 0)
    }
    fn serialize_unit_variant(self, _: &'static str, _: u32, variant: &'static str) -> Built {
        self.string(variant)
    }
    fn serialize_newtype_struct<T: serde::Serialize + ?Sized>(
        self,
        name: &'static str,
        value: &T,
    ) -> Built {
        if name.starts_with("$serde_json::private::") {
            return Err(Decline);
        }
        value.serialize(self)
    }
    fn serialize_newtype_variant<T: serde::Serialize + ?Sized>(
        self,
        _: &'static str,
        _: u32,
        variant: &'static str,
        value: &T,
    ) -> Built {
        let outer = self.open(Tag::Obj)?;
        let key = self.string(variant)?;
        self.s.stack.push(key);
        let v = value.serialize(&mut *self)?;
        self.s.stack.push(v);
        self.close(outer, 2)
    }
    fn serialize_seq(self, _: Option<usize>) -> std::result::Result<Container<'b, 's>, Decline> {
        let inner = self.open(Tag::Arr)?;
        Ok(Container {
            b: self,
            open: Open { outer: None, inner },
            per: 1,
        })
    }
    fn serialize_tuple(self, len: usize) -> std::result::Result<Container<'b, 's>, Decline> {
        self.serialize_seq(Some(len))
    }
    fn serialize_tuple_struct(
        self,
        _: &'static str,
        len: usize,
    ) -> std::result::Result<Container<'b, 's>, Decline> {
        self.serialize_seq(Some(len))
    }
    fn serialize_tuple_variant(
        self,
        _: &'static str,
        _: u32,
        variant: &'static str,
        _: usize,
    ) -> std::result::Result<Container<'b, 's>, Decline> {
        let open = self.open_variant(variant, Tag::Arr)?;
        Ok(Container {
            b: self,
            open,
            per: 1,
        })
    }
    fn serialize_map(self, _: Option<usize>) -> std::result::Result<Container<'b, 's>, Decline> {
        let inner = self.open(Tag::Obj)?;
        Ok(Container {
            b: self,
            open: Open { outer: None, inner },
            per: 2,
        })
    }
    fn serialize_struct(
        self,
        name: &'static str,
        len: usize,
    ) -> std::result::Result<Container<'b, 's>, Decline> {
        if name.starts_with("$serde_json::private::") {
            return Err(Decline);
        }
        self.serialize_map(Some(len))
    }
    fn serialize_struct_variant(
        self,
        _: &'static str,
        _: u32,
        variant: &'static str,
        _: usize,
    ) -> std::result::Result<Container<'b, 's>, Decline> {
        let open = self.open_variant(variant, Tag::Obj)?;
        Ok(Container {
            b: self,
            open,
            per: 2,
        })
    }
}

macro_rules! sequence_impls {
    ($($trait:ident { $elem:ident })*) => {$(
        impl serde::ser::$trait for Container<'_, '_> {
            type Ok = u32;
            type Error = Decline;
            fn $elem<T: serde::Serialize + ?Sized>(&mut self, value: &T) -> Step {
                self.element(value)
            }
            fn end(self) -> Built {
                self.finish()
            }
        }
    )*};
}

sequence_impls! {
    SerializeSeq { serialize_element }
    SerializeTuple { serialize_element }
    SerializeTupleStruct { serialize_field }
    SerializeTupleVariant { serialize_field }
}

impl serde::ser::SerializeMap for Container<'_, '_> {
    type Ok = u32;
    type Error = Decline;
    fn serialize_key<T: serde::Serialize + ?Sized>(&mut self, key: &T) -> Step {
        self.key(key)
    }
    fn serialize_value<T: serde::Serialize + ?Sized>(&mut self, value: &T) -> Step {
        self.element(value)
    }
    fn end(self) -> Built {
        self.finish()
    }
}

macro_rules! struct_impls {
    ($($trait:ident)*) => {$(
        impl serde::ser::$trait for Container<'_, '_> {
            type Ok = u32;
            type Error = Decline;
            fn serialize_field<T: serde::Serialize + ?Sized>(
                &mut self,
                key: &'static str,
                value: &T,
            ) -> Step {
                self.field(key, value)
            }
            fn end(self) -> Built {
                self.finish()
            }
        }
    )*};
}

struct_impls! { SerializeStruct SerializeStructVariant }

/// Map keys: strings as they are, integers and booleans as the text
/// `serde_json` writes for them; anything else declines.
struct KeyBuilder<'b, 's>(&'b mut Builder<'s>);

impl KeyBuilder<'_, '_> {
    fn display(self, v: impl std::fmt::Display) -> Built {
        use std::fmt::Write as _;
        let a = self.0.offset()?;
        write!(self.0.text, "{v}").map_err(|_| Decline)?;
        let b = self.0.offset()?;
        self.0.push(Tag::Str, a, b)
    }
}

type No = serde::ser::Impossible<u32, Decline>;

macro_rules! decline {
    ($($name:ident ( $($arg:ty),* ) -> $ret:ty;)*) => {$(
        fn $name(self, $(_: $arg),*) -> std::result::Result<$ret, Decline> {
            Err(Decline)
        }
    )*};
}

impl serde::Serializer for KeyBuilder<'_, '_> {
    type Ok = u32;
    type Error = Decline;
    type SerializeSeq = No;
    type SerializeTuple = No;
    type SerializeTupleStruct = No;
    type SerializeTupleVariant = No;
    type SerializeMap = No;
    type SerializeStruct = No;
    type SerializeStructVariant = No;

    fn serialize_str(self, v: &str) -> Built {
        self.0.string(v)
    }
    fn serialize_char(self, v: char) -> Built {
        self.0.string(v.encode_utf8(&mut [0; 4]))
    }
    fn serialize_unit_variant(self, _: &'static str, _: u32, variant: &'static str) -> Built {
        self.0.string(variant)
    }
    fn serialize_newtype_struct<T: serde::Serialize + ?Sized>(
        self,
        _: &'static str,
        value: &T,
    ) -> Built {
        value.serialize(self)
    }
    fn serialize_bool(self, v: bool) -> Built {
        self.display(v)
    }
    fn serialize_i8(self, v: i8) -> Built {
        self.display(v)
    }
    fn serialize_i16(self, v: i16) -> Built {
        self.display(v)
    }
    fn serialize_i32(self, v: i32) -> Built {
        self.display(v)
    }
    fn serialize_i64(self, v: i64) -> Built {
        self.display(v)
    }
    fn serialize_i128(self, v: i128) -> Built {
        self.display(v)
    }
    fn serialize_u8(self, v: u8) -> Built {
        self.display(v)
    }
    fn serialize_u16(self, v: u16) -> Built {
        self.display(v)
    }
    fn serialize_u32(self, v: u32) -> Built {
        self.display(v)
    }
    fn serialize_u64(self, v: u64) -> Built {
        self.display(v)
    }
    fn serialize_u128(self, v: u128) -> Built {
        self.display(v)
    }
    fn serialize_some<T: serde::Serialize + ?Sized>(self, _: &T) -> Built {
        Err(Decline)
    }
    fn serialize_newtype_variant<T: serde::Serialize + ?Sized>(
        self,
        _: &'static str,
        _: u32,
        _: &'static str,
        _: &T,
    ) -> Built {
        Err(Decline)
    }
    decline! {
        serialize_f32(f32) -> u32;
        serialize_f64(f64) -> u32;
        serialize_bytes(&[u8]) -> u32;
        serialize_none() -> u32;
        serialize_unit() -> u32;
        serialize_unit_struct(&'static str) -> u32;
        serialize_seq(Option<usize>) -> No;
        serialize_tuple(usize) -> No;
        serialize_tuple_struct(&'static str, usize) -> No;
        serialize_tuple_variant(&'static str, u32, &'static str, usize) -> No;
        serialize_map(Option<usize>) -> No;
        serialize_struct(&'static str, usize) -> No;
        serialize_struct_variant(&'static str, u32, &'static str, usize) -> No;
    }
}

/// True if any byte of `v` is `"`, `\` or a control character (< 0x20).
#[inline]
fn needs_attention(v: u64) -> bool {
    const ONES: u64 = 0x0101_0101_0101_0101;
    const HIGH: u64 = 0x8080_8080_8080_8080;
    #[inline]
    fn has_zero(x: u64) -> u64 {
        x.wrapping_sub(ONES) & !x & HIGH
    }
    let quote = has_zero(v ^ (ONES * b'"' as u64));
    let bslash = has_zero(v ^ (ONES * b'\\' as u64));
    let ctrl = v.wrapping_sub(ONES * 0x20) & !v & HIGH;
    (quote | bslash | ctrl) != 0
}

/// The index if `k` is a JS array index (canonical decimal below 2^32 - 1);
/// such keys come first, ascending, in a JS object's own-key order.
#[inline]
pub fn array_index(k: &str) -> Option<u32> {
    let b = k.as_bytes();
    if b.is_empty() || b.len() > 10 || (b.len() > 1 && b[0] == b'0') {
        return None;
    }
    let mut v: u64 = 0;
    for &c in b {
        if !c.is_ascii_digit() {
            return None;
        }
        v = v * 10 + (c - b'0') as u64;
    }
    if v < u32::MAX as u64 {
        Some(v as u32)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    fn roundtrip_str(src: &str) -> String {
        let mut s = Scratch::default();
        let t = parse(src, &mut s).unwrap();
        let r = t.root();
        let js = r.as_str();
        let mut v = Vec::new();
        js.write_to(&mut v);
        assert_eq!(js.len(), v.len());
        String::from_utf8(v).unwrap()
    }

    #[test]
    fn escapes() {
        assert_eq!(roundtrip_str(r#""a\"b\\c\/\n\u00e9""#), "a\"b\\c/\né");
        assert_eq!(roundtrip_str(r#""\ud83d\ude00""#), "\u{1F600}");
        assert_eq!(roundtrip_str(r#""\ud83dx""#), "\u{FFFD}x");
        assert_eq!(roundtrip_str(r#""\ude00""#), "\u{FFFD}");
        assert_eq!(
            roundtrip_str(r#""plain and long enough to use words""#),
            "plain and long enough to use words"
        );
    }

    #[test]
    fn rejects() {
        for bad in [
            "",
            "{",
            "[1,]",
            "{\"a\":1,}",
            "01",
            "1.",
            "-",
            "1e",
            "\"\\x\"",
            "\"\u{1}\"",
            "nul",
            "{} x",
            "{'a':1}",
            "\"\\u12\"",
        ] {
            let mut s = Scratch::default();
            assert!(parse(bad, &mut s).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn depth() {
        let ok = "[".repeat(MAX_DEPTH) + &"]".repeat(MAX_DEPTH);
        let mut s = Scratch::default();
        assert!(parse(&ok, &mut s).is_ok());
        let bad = "[".repeat(MAX_DEPTH + 1) + &"]".repeat(MAX_DEPTH + 1);
        assert!(matches!(parse(&bad, &mut s), Err(Error::DepthLimit)));
    }

    #[test]
    fn array_indices() {
        assert_eq!(array_index("0"), Some(0));
        assert_eq!(array_index("10"), Some(10));
        assert_eq!(array_index("01"), None);
        assert_eq!(array_index("4294967294"), Some(4294967294));
        assert_eq!(array_index("4294967295"), None);
        assert_eq!(array_index("-1"), None);
        assert_eq!(array_index(""), None);
    }
}
