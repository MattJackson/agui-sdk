//! protobuf `Event` -> JSON event.
//!
//! The decoder walks the message bytes in place (strings are validated and
//! handed on as borrowed slices) and drives an [`Out`] with an already ordered
//! stream of JSON tokens. Two outputs exist: a `serde_json::Value` builder and
//! a JSON text writer that never materialises a tree.

use super::codec_error::{Error, Result};
use super::json::array_index;
use super::schema::{
    Arm, Body, ContentKind, Field, Json, Label, MsgDesc, Ty, BASE_EVENT, EVENT, MAX_FIELDS,
};
use super::wire::{Reader, WireField, I64, LEN, VARINT};
use crate::pipeline::shapes;
use serde_json::{Map, Number, Value};

/// What one decoding pass produced. The event itself went to the [`Out`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Transcoded {
    /// A known event was written; this is its `type`.
    Event(&'static str),
    /// The envelope carries only an arm this build predates. Nothing was
    /// written.
    UnknownEvent {
        /// The unknown field number.
        field_number: u32,
    },
    /// The envelope carries no arm at all. Nothing was written.
    Empty,
}

/// A stream of JSON tokens.
pub(crate) trait Out {
    fn begin_obj(&mut self);
    fn end_obj(&mut self);
    fn begin_arr(&mut self);
    fn end_arr(&mut self);
    fn key(&mut self, k: &str);
    fn str(&mut self, s: &str);
    fn f64(&mut self, x: f64);
    fn i64(&mut self, x: i64);
    fn bool(&mut self, b: bool);
    fn null(&mut self);
}

pub(super) const MAX_SAFE: i64 = 9_007_199_254_740_991;

/// Prefix of the JSON spelling of an enum number with no value in this build
/// (`#9`). The encoder maps such a spelling back to its number.
pub(super) const UNKNOWN_ENUM_PREFIX: &str = "#";

// ---------------------------------------------------------------------------
// serde_json::Value output.

enum Frame {
    Obj(Map<String, Value>, Option<String>),
    Arr(Vec<Value>),
}

pub(crate) struct ValueOut {
    stack: Vec<Frame>,
    done: Option<Value>,
}

impl ValueOut {
    fn put(&mut self, v: Value) {
        match self.stack.last_mut() {
            None => self.done = Some(v),
            Some(Frame::Arr(a)) => a.push(v),
            Some(Frame::Obj(m, k)) => {
                m.insert(k.take().unwrap_or_default(), v);
            }
        }
    }
}

/// A JS number as a JSON value: integral values in the safe range become
/// integers, non-finite values become `null` (as `JSON.stringify` does).
fn js_number(x: f64) -> Value {
    if x.fract() == 0.0 && x.abs() <= MAX_SAFE as f64 {
        Value::Number(Number::from(x as i64))
    } else {
        Number::from_f64(x).map_or(Value::Null, Value::Number)
    }
}

impl Out for ValueOut {
    fn begin_obj(&mut self) {
        self.stack.push(Frame::Obj(Map::new(), None));
    }
    fn end_obj(&mut self) {
        if let Some(Frame::Obj(m, _)) = self.stack.pop() {
            self.put(Value::Object(m));
        }
    }
    fn begin_arr(&mut self) {
        self.stack.push(Frame::Arr(Vec::new()));
    }
    fn end_arr(&mut self) {
        if let Some(Frame::Arr(a)) = self.stack.pop() {
            self.put(Value::Array(a));
        }
    }
    fn key(&mut self, k: &str) {
        if let Some(Frame::Obj(_, slot)) = self.stack.last_mut() {
            *slot = Some(k.to_owned());
        }
    }
    fn str(&mut self, s: &str) {
        self.put(Value::String(s.to_owned()));
    }
    fn f64(&mut self, x: f64) {
        self.put(js_number(x));
    }
    fn i64(&mut self, x: i64) {
        // Exact whatever the magnitude: a value beyond the safe range is
        // left for enforcement to judge, as it is over SSE.
        self.put(Value::Number(Number::from(x)));
    }
    fn bool(&mut self, b: bool) {
        self.put(Value::Bool(b));
    }
    fn null(&mut self) {
        self.put(Value::Null);
    }
}

// ---------------------------------------------------------------------------
// Container-entry budget.

/// Charges one unit per value token (`begin_obj`, `begin_arr`, `str`, `f64`,
/// `i64`, `bool`, `null`) and stops forwarding to the inner output once the
/// budget is spent.
///
/// Every object member has exactly one value and every array element is one
/// value, so the value tokens of an event, less its root, are its object
/// members plus array elements: the count the tree enforcer charges against
/// `Limits::max_container_entries`. The inner output therefore never grows
/// past what enforcement would accept, however small the frame that asked
/// for it.
struct Budget<'o, O: Out> {
    inner: &'o mut O,
    left: usize,
    over: bool,
}

impl<'o, O: Out> Budget<'o, O> {
    fn new(inner: &'o mut O, max_entries: usize) -> Self {
        Budget {
            inner,
            // The root value is not an entry.
            left: max_entries.saturating_add(1),
            over: false,
        }
    }

    /// Whether the next value token may be forwarded.
    #[inline]
    fn charge(&mut self) -> bool {
        if self.over {
            return false;
        }
        if self.left == 0 {
            self.over = true;
            return false;
        }
        self.left -= 1;
        true
    }
}

impl<O: Out> Out for Budget<'_, O> {
    fn begin_obj(&mut self) {
        if self.charge() {
            self.inner.begin_obj();
        }
    }
    fn end_obj(&mut self) {
        if !self.over {
            self.inner.end_obj();
        }
    }
    fn begin_arr(&mut self) {
        if self.charge() {
            self.inner.begin_arr();
        }
    }
    fn end_arr(&mut self) {
        if !self.over {
            self.inner.end_arr();
        }
    }
    fn key(&mut self, k: &str) {
        if !self.over {
            self.inner.key(k);
        }
    }
    fn str(&mut self, s: &str) {
        if self.charge() {
            self.inner.str(s);
        }
    }
    fn f64(&mut self, x: f64) {
        if self.charge() {
            self.inner.f64(x);
        }
    }
    fn i64(&mut self, x: i64) {
        if self.charge() {
            self.inner.i64(x);
        }
    }
    fn bool(&mut self, b: bool) {
        if self.charge() {
            self.inner.bool(b);
        }
    }
    fn null(&mut self) {
        if self.charge() {
            self.inner.null();
        }
    }
}

// ---------------------------------------------------------------------------
// JSON text output.

pub(crate) struct TextOut<'s> {
    pub(crate) s: &'s mut String,
    comma: bool,
}

impl<'s> TextOut<'s> {
    pub(crate) fn new(s: &'s mut String) -> Self {
        TextOut { s, comma: false }
    }
    #[inline]
    fn sep(&mut self) {
        if self.comma {
            self.s.push(',');
        }
    }
    fn quoted(&mut self, v: &str) {
        self.s.reserve(v.len() + 2);
        self.s.push('"');
        let b = v.as_bytes();
        if !needs_escaping(b) {
            // Most text: one copy.
            self.s.push_str(v);
            self.s.push('"');
            return;
        }
        let mut run = 0;
        let mut i = 0;
        while i < b.len() {
            let c = b[i];
            i += 1;
            let esc: Option<&str> = match c {
                b'"' => Some("\\\""),
                b'\\' => Some("\\\\"),
                b'\n' => Some("\\n"),
                b'\r' => Some("\\r"),
                b'\t' => Some("\\t"),
                0x08 => Some("\\b"),
                0x0c => Some("\\f"),
                c if c < 0x20 => None,
                _ => continue,
            };
            self.s.push_str(&v[run..i - 1]);
            match esc {
                Some(e) => self.s.push_str(e),
                None => {
                    const HEX: &[u8; 16] = b"0123456789abcdef";
                    self.s.push_str("\\u00");
                    self.s.push(HEX[(c >> 4) as usize] as char);
                    self.s.push(HEX[(c & 15) as usize] as char);
                }
            }
            run = i;
        }
        self.s.push_str(&v[run..]);
        self.s.push('"');
    }
}

/// Whether `text` holds a byte [`TextOut::quoted`] escapes, eight bytes at
/// a time (the tail padded with spaces, which need none).
#[inline]
fn needs_escaping(text: &[u8]) -> bool {
    let mut words = text.chunks_exact(8);
    for chunk in &mut words {
        let mut word = [0; 8];
        word.copy_from_slice(chunk);
        if escapes_any(u64::from_le_bytes(word)) {
            return true;
        }
    }
    let tail = words.remainder();
    let mut word = [b' '; 8];
    word[..tail.len()].copy_from_slice(tail);
    escapes_any(u64::from_le_bytes(word))
}

/// Whether any byte of `word` is one [`TextOut::quoted`] escapes: a control
/// character, `"` or `\\` (exact, by the usual bytewise-zero tests).
#[inline]
fn escapes_any(word: u64) -> bool {
    const ONES: u64 = 0x0101_0101_0101_0101;
    const HIGH: u64 = 0x8080_8080_8080_8080;
    let zero_in = |x: u64| x.wrapping_sub(ONES) & !x & HIGH != 0;
    let below_space = word.wrapping_sub(ONES * 0x20) & !word & HIGH != 0;
    below_space
        || zero_in(word ^ (ONES * u64::from(b'"')))
        || zero_in(word ^ (ONES * u64::from(b'\\')))
}

impl Out for TextOut<'_> {
    fn begin_obj(&mut self) {
        self.sep();
        self.s.push('{');
        self.comma = false;
    }
    fn end_obj(&mut self) {
        self.s.push('}');
        self.comma = true;
    }
    fn begin_arr(&mut self) {
        self.sep();
        self.s.push('[');
        self.comma = false;
    }
    fn end_arr(&mut self) {
        self.s.push(']');
        self.comma = true;
    }
    fn key(&mut self, k: &str) {
        self.sep();
        self.quoted(k);
        self.s.push(':');
        self.comma = false;
    }
    fn str(&mut self, v: &str) {
        self.sep();
        self.quoted(v);
        self.comma = true;
    }
    fn f64(&mut self, x: f64) {
        self.sep();
        write_js_number(self.s, x);
        self.comma = true;
    }
    fn i64(&mut self, x: i64) {
        self.sep();
        write_int(self.s, x);
        self.comma = true;
    }
    fn bool(&mut self, b: bool) {
        self.sep();
        self.s.push_str(if b { "true" } else { "false" });
        self.comma = true;
    }
    fn null(&mut self) {
        self.sep();
        self.s.push_str("null");
        self.comma = true;
    }
}

fn write_int(s: &mut String, x: i64) {
    let mut buf = [0u8; 20];
    let mut i = buf.len();
    let neg = x < 0;
    let mut u = x.unsigned_abs();
    loop {
        i -= 1;
        buf[i] = b'0' + (u % 10) as u8;
        u /= 10;
        if u == 0 {
            break;
        }
    }
    if neg {
        s.push('-');
    }
    for &c in &buf[i..] {
        s.push(c as char);
    }
}

/// Formats `x` exactly as JS `Number.prototype.toString` does (with
/// non-finite values as `null`, as `JSON.stringify` does).
pub fn write_js_number(s: &mut String, x: f64) {
    if !x.is_finite() {
        s.push_str("null");
        return;
    }
    if x == 0.0 {
        s.push('0');
        return;
    }
    if x.fract() == 0.0 && x.abs() <= MAX_SAFE as f64 {
        write_int(s, x as i64);
        return;
    }
    // Shortest round-trip digits via `{:e}` ("d.ddde-7"), formatted into a
    // stack buffer.
    use std::fmt::Write as _;
    let mut tmp = StackBuf::default();
    let _ = write!(tmp, "{:e}", x.abs());
    let tmp = tmp.as_str();
    let (mant, exp) = tmp.split_once('e').unwrap_or((tmp, "0"));
    let exp: i32 = exp.parse().unwrap_or(0);
    let mut dbuf = [0u8; 32];
    let mut k = 0;
    for &c in mant.as_bytes() {
        if c != b'.' && k < dbuf.len() {
            dbuf[k] = c;
            k += 1;
        }
    }
    let digits = std::str::from_utf8(&dbuf[..k]).unwrap_or("0");
    let k = k as i32;
    let n = exp + 1;
    if x < 0.0 {
        s.push('-');
    }
    if k <= n && n <= 21 {
        s.push_str(digits);
        for _ in 0..(n - k) {
            s.push('0');
        }
    } else if 0 < n && n <= 21 {
        s.push_str(&digits[..n as usize]);
        s.push('.');
        s.push_str(&digits[n as usize..]);
    } else if -6 < n && n <= 0 {
        s.push_str("0.");
        for _ in 0..(-n) {
            s.push('0');
        }
        s.push_str(digits);
    } else {
        s.push_str(&digits[..1]);
        if k > 1 {
            s.push('.');
            s.push_str(&digits[1..]);
        }
        s.push('e');
        s.push(if n > 0 { '+' } else { '-' });
        write_int(s, (n - 1).abs() as i64);
    }
}

/// A fixed-capacity `fmt::Write` target (an `f64` in `{:e}` form fits).
struct StackBuf {
    b: [u8; 40],
    n: usize,
}

impl Default for StackBuf {
    fn default() -> Self {
        StackBuf { b: [0; 40], n: 0 }
    }
}

impl StackBuf {
    fn as_str(&self) -> &str {
        std::str::from_utf8(&self.b[..self.n]).unwrap_or("0e0")
    }
}

impl std::fmt::Write for StackBuf {
    fn write_str(&mut self, s: &str) -> std::fmt::Result {
        let end = self.n + s.len();
        if end > self.b.len() {
            return Err(std::fmt::Error);
        }
        self.b[self.n..end].copy_from_slice(s.as_bytes());
        self.n = end;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Decoder.

#[derive(Clone, Copy, Default)]
struct Occ {
    count: u32,
    /// Last occurrence (payload range for LEN, value for VARINT/I64).
    last: WireField,
    /// Absolute start offset of the first occurrence in the current run of
    /// this field (a oneof arm switch starts a new run); occurrences before
    /// it were cleared by the switch and are not merged. 0 for fields that
    /// are not oneof members: merge everything. Relies on [`Fields`]
    /// yielding fields in increasing absolute offset.
    from: usize,
}

fn utf8(b: &[u8]) -> Result<&str> {
    std::str::from_utf8(b).map_err(|_| Error::InvalidUtf8)
}

/// Deepest container nesting the decoder produces, the event object counted
/// as 1: a fixed stack-safety ceiling, not the configured `Limits::max_depth`
/// (which enforcement applies after decoding). It equals the pipeline's
/// default `max_depth`, which counts the same way, so at the default the
/// decoder accepts exactly the nesting enforcement accepts. Far
/// shallower than the encoder's [`MAX_DEPTH`](super::json::MAX_DEPTH): each
/// level is a few stack frames of recursion on whatever thread decodes.
pub(super) const DECODE_MAX_DEPTH: usize = 128;

/// Where a message's bytes are. Every field position is absolute in the frame being
/// decoded, so nested messages are read where they lie: either one range of
/// the frame, or, for a singular message field that occurs several times
/// (protobuf merges the occurrences), the ranges of every occurrence, held
/// in [`Dec::spans`]. A merge never copies payload bytes.
#[derive(Clone, Copy)]
enum Src {
    /// One range of the frame.
    One(usize, usize),
    /// `Dec::spans[a..b]`, read in order as one message.
    Many(usize, usize),
}

impl Src {
    fn of(f: &WireField) -> Src {
        Src::One(f.start, f.end)
    }
}

/// The fields of a [`Src`], in wire order, positions absolute in the frame.
struct Fields<'b> {
    root: &'b [u8],
    r: Reader<'b>,
    next: usize,
    end: usize,
}

impl<'b> Fields<'b> {
    fn new(root: &'b [u8], body: Src) -> Self {
        match body {
            Src::One(s, e) => Fields {
                root,
                r: Reader::span(root, s, e),
                next: 0,
                end: 0,
            },
            Src::Many(a, b) => Fields {
                root,
                r: Reader::span(root, 0, 0),
                next: a,
                end: b,
            },
        }
    }

    fn next(&mut self, spans: &[[u32; 2]]) -> Result<Option<WireField>> {
        loop {
            if let Some(f) = self.r.next_field()? {
                return Ok(Some(f));
            }
            let Some(&[s, e]) = spans.get(self.next).filter(|_| self.next < self.end) else {
                return Ok(None);
            };
            self.next += 1;
            self.r = Reader::span(self.root, s as usize, e as usize);
        }
    }
}

/// The definition each message descriptor maps to ([`shapes::lookup`] by
/// name), remembered per descriptor: decoding asks for it once per message,
/// and a binary search over every definition name each time is wasted work.
/// Direct-mapped on the descriptor's address; a collision just looks up again.
struct DefCache {
    slots: [(usize, Option<shapes::DefId>); DEF_CACHE],
}

const DEF_CACHE: usize = 32;

impl DefCache {
    fn new() -> Self {
        DefCache {
            slots: [(0, None); DEF_CACHE],
        }
    }

    fn lookup(&mut self, desc: &MsgDesc) -> Option<shapes::DefId> {
        let addr = desc as *const MsgDesc as usize;
        let slot = &mut self.slots[(addr / std::mem::size_of::<MsgDesc>()) % DEF_CACHE];
        if slot.0 != addr {
            *slot = (addr, shapes::lookup(desc.name));
        }
        slot.1
    }
}

/// A selected oneof arm and its (possibly merged) payload.
type Selected = Option<(&'static Arm, Src)>;

pub(crate) struct Dec {
    /// The `Value` builder's container stack, kept between calls.
    frames: Vec<Frame>,
    /// Struct entries in progress: [key start, key end, entry start, entry
    /// end]; key start `u32::MAX` marks a dropped duplicate.
    ents: Vec<[u32; 4]>,
    /// Scratch for duplicate detection in large structs.
    sort: Vec<u32>,
    defs: DefCache,
    /// Occurrence ranges of merged message fields ([`Src::Many`]), used as
    /// a stack: each merge pushes its ranges and truncates them when done.
    spans: Vec<[u32; 2]>,
}

/// The JSON-schema object a decoded wire message becomes, from the generated
/// enforcement table: the definition of the same name, and for a union keyed
/// by a member the message carries (`Message` by `role`), the arm that
/// member's value selects. `None` when the schema has no such object (an
/// unknown role, a wire-only message); then nothing is materialised.
fn json_object(
    defs: &mut DefCache,
    desc: &MsgDesc,
    fs: &[Field],
    occ: &[Occ],
    root: &[u8],
) -> Result<Option<&'static shapes::Object>> {
    let Some(id) = defs.lookup(desc) else {
        return Ok(None);
    };
    Ok(match id.def().kind {
        shapes::DefKind::Object(ref o) => Some(o),
        shapes::DefKind::Union(u) => {
            let at = fs
                .iter()
                .position(|f| f.json == Json::Key(u.discriminator) && matches!(f.ty, Ty::String));
            match at {
                Some(i) if occ[i].count > 0 => {
                    let w = occ[i].last;
                    let v = utf8(&root[w.start..w.end])?;
                    u.arm(v).and_then(|d| match d.def().kind {
                        shapes::DefKind::Object(ref o) => Some(o),
                        _ => None,
                    })
                }
                _ => None,
            }
        }
        _ => None,
    })
}

/// Whether `key` is a required member of `obj` whose schema admits an array:
/// the members an absent repeated field must decode to `[]` for.
fn requires_list(obj: Option<&'static shapes::Object>, key: &str) -> bool {
    obj.and_then(|o| o.field(key))
        .is_some_and(|f| f.required && admits_array(&f.ty, 0))
}

fn admits_array(ty: &shapes::Ty, depth: u8) -> bool {
    match ty {
        shapes::Ty::Array { .. } => true,
        shapes::Ty::OneOf(arms) => arms.iter().any(|t| admits_array(t, depth)),
        // Aliases are shallow; the bound only guards a malformed table.
        shapes::Ty::Ref(id) if depth < 8 => match id.def().kind {
            shapes::DefKind::Alias(ref t) => admits_array(t, depth + 1),
            _ => false,
        },
        _ => false,
    }
}

/// Whether the message at `f` carries one of `arms` (wire type LEN).
fn has_arm(root: &[u8], f: &WireField, arms: &'static [Arm]) -> Result<bool> {
    let mut r = Reader::span(root, f.start, f.end);
    while let Some(g) = r.next_field()? {
        if g.wt == LEN && arms.iter().any(|a| a.number == g.number) {
            return Ok(true);
        }
    }
    Ok(false)
}

impl Dec {
    /// Releases the capacity a large event grew the scratch vectors to.
    fn trim(&mut self) {
        self.sort.clear();
        if self.ents.capacity() > SCRATCH_KEEP_ENTRIES {
            self.ents.shrink_to(SCRATCH_KEEP_ENTRIES);
        }
        if self.sort.capacity() > SCRATCH_KEEP_ENTRIES {
            self.sort.shrink_to(SCRATCH_KEEP_ENTRIES);
        }
        if self.spans.capacity() > SCRATCH_KEEP_ENTRIES {
            self.spans.shrink_to(SCRATCH_KEEP_ENTRIES);
        }
    }

    pub(crate) fn new() -> Self {
        Dec {
            frames: Vec::new(),
            ents: Vec::new(),
            sort: Vec::new(),
            defs: DefCache::new(),
            spans: Vec::new(),
        }
    }

    /// Decodes an `Event` envelope into a `serde_json::Value` (`None` unless
    /// a known event was decoded), reusing the builder's stack.
    pub(super) fn event_value(
        &mut self,
        buf: &[u8],
        max_entries: usize,
    ) -> Result<(Transcoded, Option<Value>)> {
        let mut out = ValueOut {
            stack: std::mem::take(&mut self.frames),
            done: None,
        };
        let r = self.event(buf, &mut out, max_entries);
        out.stack.clear();
        self.frames = out.stack;
        Ok((r?, out.done))
    }

    /// Decodes an `Event` envelope, producing at most `max_entries`
    /// container entries (object members plus array elements; see
    /// [`Budget`]); [`Error::EntryLimit`] beyond that.
    pub(crate) fn event<O: Out>(
        &mut self,
        buf: &[u8],
        out: &mut O,
        max_entries: usize,
    ) -> Result<Transcoded> {
        if !positions_fit(buf.len()) {
            return Err(Error::InputTooLarge);
        }
        self.spans.clear();
        let mut budget = Budget::new(out, max_entries);
        let r = self.event_in(buf, &mut budget);
        let over = budget.over;
        self.spans.clear();
        if over {
            return Err(Error::EntryLimit { max: max_entries });
        }
        r
    }

    fn event_in<O: Out>(&mut self, root: &[u8], out: &mut O) -> Result<Transcoded> {
        let Body::Union { arms, .. } = EVENT.body else {
            unreachable!("Event is a union")
        };
        let (arm, unknown) = self.select_arm(root, Src::One(0, root.len()), arms)?;
        let Some((arm, body)) = arm else {
            return Ok(match unknown {
                Some(n) => Transcoded::UnknownEvent { field_number: n },
                None => Transcoded::Empty,
            });
        };
        out.begin_obj();
        out.key("type");
        out.str(arm.json);
        self.members(arm.msg, root, body, out, 1)?;
        out.end_obj();
        Ok(Transcoded::Event(arm.json))
    }

    /// The payload of a singular message-typed field of `parent`: the last
    /// occurrence, or all occurrences merged when there are several (their
    /// ranges pushed on [`Dec::spans`]; the caller truncates them).
    fn payload(&mut self, root: &[u8], parent: Src, o: &Occ) -> Result<Src> {
        if o.count <= 1 {
            return Ok(Src::of(&o.last));
        }
        let at = self.spans.len();
        let mut r = Fields::new(root, parent);
        while let Some(f) = r.next(&self.spans)? {
            // Empty occurrences contribute nothing to a merge.
            if f.number == o.last.number && f.wt == LEN && f.end > f.start && f.start >= o.from {
                self.spans.push([f.start as u32, f.end as u32]);
            }
        }
        Ok(Src::Many(at, self.spans.len()))
    }

    /// Selects the arm of a oneof-only message: the last known arm wins, and
    /// repeated occurrences of it are merged. Returns the last unknown field
    /// number when no known arm is present.
    fn select_arm(
        &mut self,
        root: &[u8],
        body: Src,
        arms: &'static [Arm],
    ) -> Result<(Selected, Option<u32>)> {
        let mut r = Fields::new(root, body);
        let mut cur: Option<(&'static Arm, Occ)> = None;
        let mut unknown = None;
        while let Some(f) = r.next(&self.spans)? {
            match arms.iter().find(|a| a.number == f.number) {
                Some(a) if f.wt == LEN => match &mut cur {
                    Some((ca, o)) if ca.number == a.number => {
                        o.count += 1;
                        o.last = f;
                    }
                    _ => {
                        cur = Some((
                            a,
                            Occ {
                                count: 1,
                                last: f,
                                from: f.start,
                            },
                        ));
                    }
                },
                _ => unknown = Some(f.number),
            }
        }
        match cur {
            Some((a, o)) => Ok((Some((a, self.payload(root, body, &o)?)), unknown)),
            None => Ok((None, unknown)),
        }
    }

    /// Whether repeated field `f` (occurrences `o`) yields any element:
    /// elements of a union whose arm this build predates are dropped, and a
    /// list left empty that way is absent, as protobuf would re-encode it.
    fn any_element(&self, root: &[u8], body: Src, f: &Field, o: &Occ) -> Result<bool> {
        if o.count == 0 {
            return Ok(false);
        }
        let Ty::Message(MsgDesc {
            body: Body::Union { arms, .. },
            ..
        }) = f.ty
        else {
            return Ok(true);
        };
        let mut r = Fields::new(root, body);
        while let Some(w) = r.next(&self.spans)? {
            if w.number == f.number && w.wt == LEN && has_arm(root, &w, arms)? {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// Emits the members of a plain message (no braces).
    fn members<O: Out>(
        &mut self,
        desc: &MsgDesc,
        root: &[u8],
        body: Src,
        out: &mut O,
        depth: usize,
    ) -> Result<()> {
        if depth > DECODE_MAX_DEPTH {
            return Err(Error::DepthLimit);
        }
        let fs = desc.fields();
        let mut occ = [Occ::default(); MAX_FIELDS];
        let mut r = Fields::new(root, body);
        while let Some(f) = r.next(&self.spans)? {
            if let Some(i) = fs.iter().position(|d| d.number == f.number) {
                if fs[i].ty.wire_type() == f.wt {
                    occ[i].count += 1;
                    occ[i].last = f;
                }
            }
        }

        let schema = json_object(&mut self.defs, desc, fs, &occ, root)?;
        let mut content_done = false;
        for (i, f) in fs.iter().enumerate() {
            let o = &occ[i];
            match f.json {
                Json::Base | Json::EventType | Json::OutcomeKey(_) => {}
                Json::Key(k) => {
                    let list_required = f.label == Label::Repeated && requires_list(schema, k);
                    self.member(f, k, root, body, o, out, depth, list_required)?
                }
                Json::Content(_) => {
                    if content_done {
                        continue;
                    }
                    content_done = true;
                    let mut pick = None;
                    for want in [ContentKind::Object, ContentKind::Parts, ContentKind::Text] {
                        let Some(j) = fs.iter().position(|g| g.json == Json::Content(want)) else {
                            continue;
                        };
                        let present = if fs[j].label == Label::Repeated {
                            self.any_element(root, body, &fs[j], &occ[j])?
                        } else {
                            occ[j].count > 0
                        };
                        if present {
                            pick = Some(j);
                            break;
                        }
                    }
                    if let Some(j) = pick {
                        self.member(&fs[j], "content", root, body, &occ[j], out, depth, false)?;
                    } else if requires_list(schema, "content") {
                        // No content arm on the wire. The text and object
                        // arms have explicit presence, so an empty one would
                        // still be there; what protobuf cannot carry is an
                        // empty part list. Where the role requires content
                        // and admits a list, that is what was sent (PROC-09).
                        out.key("content");
                        out.begin_arr();
                        out.end_arr();
                    }
                }
                Json::OutcomeType => {
                    if o.count == 0 {
                        continue;
                    }
                    let t = utf8(&root[o.last.start..o.last.end])?;
                    if t.is_empty() {
                        continue;
                    }
                    if depth + 1 > DECODE_MAX_DEPTH {
                        return Err(Error::DepthLimit);
                    }
                    out.key("outcome");
                    out.begin_obj();
                    out.key("type");
                    out.str(t);
                    for (j, g) in fs.iter().enumerate() {
                        if let Json::OutcomeKey(k) = g.json {
                            self.member(g, k, root, body, &occ[j], out, depth + 1, false)?;
                        }
                    }
                    out.end_obj();
                }
            }
        }
        // Base-event members go last, flattened into this object.
        if let Some(i) = fs.iter().position(|f| f.json == Json::Base) {
            if occ[i].count > 0 {
                let mark = self.spans.len();
                let inner = self.payload(root, body, &occ[i])?;
                let r = self.members(&BASE_EVENT, root, inner, out, depth);
                self.spans.truncate(mark);
                r?;
            }
        }
        Ok(())
    }

    /// Emits `key: value` for one field, or nothing when it is absent. An
    /// absent repeated field is emitted as `[]` when `list_required` (the
    /// JSON schema requires the member, and protobuf cannot tell an empty
    /// list from an omitted one); a repeated field whose every element is
    /// dropped counts as absent.
    #[allow(clippy::too_many_arguments)]
    fn member<O: Out>(
        &mut self,
        f: &Field,
        key: &str,
        root: &[u8],
        body: Src,
        o: &Occ,
        out: &mut O,
        depth: usize,
        list_required: bool,
    ) -> Result<()> {
        if f.label == Label::Repeated {
            let any = self.any_element(root, body, f, o)?;
            if !any && !list_required {
                return Ok(());
            }
            if depth + 1 > DECODE_MAX_DEPTH {
                return Err(Error::DepthLimit);
            }
            out.key(key);
            out.begin_arr();
            if any {
                let mut r = Fields::new(root, body);
                while let Some(w) = r.next(&self.spans)? {
                    if w.number != f.number || w.wt != f.ty.wire_type() {
                        continue;
                    }
                    if let Ty::Message(MsgDesc {
                        body: Body::Union { arms, .. },
                        ..
                    }) = f.ty
                    {
                        // An element whose arm this build predates is dropped.
                        let mark = self.spans.len();
                        let r = match self.select_arm(root, Src::of(&w), arms) {
                            Ok((Some((arm, inner)), _)) => {
                                self.union_body(arm, root, inner, out, depth + 2)
                            }
                            Ok((None, _)) => Ok(()),
                            Err(e) => Err(e),
                        };
                        self.spans.truncate(mark);
                        r?;
                        continue;
                    }
                    self.single(
                        f,
                        root,
                        body,
                        &Occ {
                            count: 1,
                            last: w,
                            from: 0,
                        },
                        out,
                        depth + 1,
                    )?;
                }
            }
            out.end_arr();
            return Ok(());
        }
        match f.ty {
            Ty::Enum(_) => {}
            // Every implicit-presence string is one the JSON schema requires.
            // proto3 cannot tell it omitted from empty, so it arrives as ""
            // (PROC-09), never silently absent.
            Ty::String
                if f.label == Label::Implicit && (o.count == 0 || o.last.start == o.last.end) =>
            {
                out.key(key);
                out.str("");
                return Ok(());
            }
            _ if o.count == 0 => return Ok(()),
            Ty::Message(m) => {
                // A union with no known arm is left out entirely.
                if let Body::Union { arms, .. } = m.body {
                    let mark = self.spans.len();
                    let r = self.union_member(key, root, body, o, arms, out, depth);
                    self.spans.truncate(mark);
                    return r;
                }
            }
            _ => {}
        }
        out.key(key);
        self.single(f, root, body, o, out, depth)
    }

    /// A singular union-typed member: `key` and the selected arm, or nothing
    /// when no known arm is present.
    #[allow(clippy::too_many_arguments)]
    fn union_member<O: Out>(
        &mut self,
        key: &str,
        root: &[u8],
        body: Src,
        o: &Occ,
        arms: &'static [Arm],
        out: &mut O,
        depth: usize,
    ) -> Result<()> {
        let inner = self.payload(root, body, o)?;
        let (arm, _) = self.select_arm(root, inner, arms)?;
        if let Some((arm, inner)) = arm {
            out.key(key);
            return self.union_body(arm, root, inner, out, depth + 1);
        }
        Ok(())
    }

    fn union_body<O: Out>(
        &mut self,
        arm: &Arm,
        root: &[u8],
        body: Src,
        out: &mut O,
        depth: usize,
    ) -> Result<()> {
        if depth > DECODE_MAX_DEPTH {
            return Err(Error::DepthLimit);
        }
        out.begin_obj();
        out.key("type");
        out.str(arm.json);
        self.members(arm.msg, root, body, out, depth)?;
        out.end_obj();
        Ok(())
    }

    /// Emits one value of field `f` (occurrence `o` within `parent`; for a
    /// singular message-like field with several occurrences, they are
    /// merged).
    fn single<O: Out>(
        &mut self,
        f: &Field,
        root: &[u8],
        parent: Src,
        o: &Occ,
        out: &mut O,
        depth: usize,
    ) -> Result<()> {
        let w = o.last;
        match f.ty {
            Ty::String => out.str(utf8(&root[w.start..w.end])?),
            Ty::Bool => out.bool(w.v != 0),
            // The wire carries any 64-bit value. It is handed on exactly and
            // enforcement judges the range after middleware, as on SSE
            // (PROC-06).
            Ty::Int64 => out.i64(w.v as i64),
            Ty::Enum(e) => {
                // An enum is an `int32`: a wider varint is read as its low
                // 32 bits (truncating on purpose), as protobuf parsers do, so
                // the number spelled below is one the encoder writes back.
                let n = if o.count == 0 { 0 } else { w.v as i32 };
                match e.by_number(n) {
                    Some(v) => out.str(v.json),
                    // A value this build predates is not malformed bytes: it
                    // is spelled `#<number>`, a string no enum value uses, so
                    // enforcement drops the element with the same warning an
                    // unknown spelling gets on the JSON wire (D3, PROC-06).
                    None => {
                        use std::fmt::Write as _;
                        let mut b = StackBuf::default();
                        let _ = write!(b, "{UNKNOWN_ENUM_PREFIX}{n}");
                        out.str(b.as_str());
                    }
                }
            }
            Ty::Message(_) | Ty::Struct | Ty::Value => {
                let mark = self.spans.len();
                let r = self.message_value(f, root, parent, o, out, depth);
                self.spans.truncate(mark);
                r?;
            }
        }
        Ok(())
    }

    /// [`single`](Self::single) for the message-like types.
    fn message_value<O: Out>(
        &mut self,
        f: &Field,
        root: &[u8],
        parent: Src,
        o: &Occ,
        out: &mut O,
        depth: usize,
    ) -> Result<()> {
        let body = self.payload(root, parent, o)?;
        match f.ty {
            Ty::Message(m) => match m.body {
                Body::Fields(_) => {
                    if depth + 1 > DECODE_MAX_DEPTH {
                        return Err(Error::DepthLimit);
                    }
                    out.begin_obj();
                    self.members(m, root, body, out, depth + 1)?;
                    out.end_obj();
                }
                Body::Union { arms, .. } => {
                    // `member` filters out unknown arms before calling
                    // here; an empty object is the defensive fallback.
                    let (arm, _) = self.select_arm(root, body, arms)?;
                    match arm {
                        Some((arm, inner)) => self.union_body(arm, root, inner, out, depth + 1)?,
                        None => {
                            out.begin_obj();
                            out.end_obj();
                        }
                    }
                }
            },
            Ty::Struct => self.structure(root, body, out, depth + 1)?,
            _ => self.value(root, body, out, depth)?,
        }
        Ok(())
    }

    /// Emits a `google.protobuf.Value`. `depth` is that of the enclosing
    /// container; objects and arrays nest one deeper.
    fn value<O: Out>(&mut self, root: &[u8], body: Src, out: &mut O, depth: usize) -> Result<()> {
        let mut r = Fields::new(root, body);
        let mut kind: Option<Occ> = None;
        while let Some(f) = r.next(&self.spans)? {
            let ok = match f.number {
                1 | 4 => f.wt == VARINT,
                2 => f.wt == I64,
                3 | 5 | 6 => f.wt == LEN,
                _ => false,
            };
            if !ok {
                continue;
            }
            match &mut kind {
                Some(k) if k.last.number == f.number => {
                    k.count += 1;
                    k.last = f;
                }
                _ => {
                    kind = Some(Occ {
                        count: 1,
                        last: f,
                        from: f.start,
                    });
                }
            }
        }
        let Some(k) = kind else {
            out.null();
            return Ok(());
        };
        let w = k.last;
        match w.number {
            1 => out.null(),
            2 => out.f64(f64::from_bits(w.v)),
            3 => out.str(utf8(&root[w.start..w.end])?),
            4 => out.bool(w.v != 0),
            n => {
                let mark = self.spans.len();
                let r = match self.payload(root, body, &k) {
                    Ok(inner) if n == 5 => self.structure(root, inner, out, depth + 1),
                    Ok(inner) => self.list(root, inner, out, depth + 1),
                    Err(e) => Err(e),
                };
                self.spans.truncate(mark);
                r?;
            }
        }
        Ok(())
    }

    fn list<O: Out>(&mut self, root: &[u8], body: Src, out: &mut O, depth: usize) -> Result<()> {
        if depth > DECODE_MAX_DEPTH {
            return Err(Error::DepthLimit);
        }
        out.begin_arr();
        let mut r = Fields::new(root, body);
        while let Some(f) = r.next(&self.spans)? {
            if f.number == 1 && f.wt == LEN {
                self.value(root, Src::of(&f), out, depth)?;
            }
        }
        out.end_arr();
        Ok(())
    }

    /// Emits a `google.protobuf.Struct` in JS own-key order: array-index keys
    /// ascending, then the rest in wire order; a repeated key keeps its first
    /// position and its last value.
    fn structure<O: Out>(
        &mut self,
        root: &[u8],
        body: Src,
        out: &mut O,
        depth: usize,
    ) -> Result<()> {
        if depth > DECODE_MAX_DEPTH {
            return Err(Error::DepthLimit);
        }
        let base = self.ents.len();
        let r = self.collect_entries(root, body, base);
        if let Err(e) = r {
            self.ents.truncate(base);
            return Err(e);
        }
        let r = self.emit_entries(root, base, out, depth);
        self.ents.truncate(base);
        r
    }

    fn collect_entries(&mut self, root: &[u8], body: Src, base: usize) -> Result<()> {
        let mut r = Fields::new(root, body);
        let mut use_sorted_dup_check = false;
        while let Some(f) = r.next(&self.spans)? {
            if f.number != 1 || f.wt != LEN {
                continue;
            }
            let mut er = Reader::span(root, f.start, f.end);
            let (mut ks, mut ke) = (0, 0);
            while let Some(g) = er.next_field()? {
                if g.number == 1 && g.wt == LEN {
                    ks = g.start;
                    ke = g.end;
                }
            }
            utf8(&root[ks..ke])?;
            self.ents
                .push([ks as u32, ke as u32, f.start as u32, f.end as u32]);
            if self.ents.len() - base > SORTED_DUP_CHECK_ABOVE {
                use_sorted_dup_check = true;
            }
        }
        // Duplicate keys: keep the first position, take the last value.
        let n = self.ents.len() - base;
        if n > 1 {
            if use_sorted_dup_check {
                // Sort entry indices by key (stable, so equal keys stay in
                // wire order); each run of equal keys keeps its first
                // position and takes its last value.
                let mut order = std::mem::take(&mut self.sort);
                order.clear();
                order.extend(base as u32..self.ents.len() as u32);
                fn key<'k>(ents: &[[u32; 4]], buf: &'k [u8], i: u32) -> &'k [u8] {
                    let e = ents[i as usize];
                    &buf[e[0] as usize..e[1] as usize]
                }
                let ents = &self.ents;
                order.sort_by(|&a, &b| key(ents, root, a).cmp(key(ents, root, b)));
                let mut g = 0;
                while g < order.len() {
                    let mut h = g + 1;
                    let kg = key(&self.ents, root, order[g]);
                    while h < order.len() && key(&self.ents, root, order[h]) == kg {
                        h += 1;
                    }
                    if h - g > 1 {
                        let (first, last) = (order[g] as usize, order[h - 1] as usize);
                        let [_, _, es, ee] = self.ents[last];
                        self.ents[first][2] = es;
                        self.ents[first][3] = ee;
                        for &dup in &order[g + 1..h] {
                            self.ents[dup as usize][0] = u32::MAX;
                        }
                    }
                    g = h;
                }
                self.sort = order;
            } else {
                for i in base + 1..self.ents.len() {
                    let [ks, ke, es, ee] = self.ents[i];
                    let k = &root[ks as usize..ke as usize];
                    for j in base..i {
                        let [js, je, _, _] = self.ents[j];
                        if js != u32::MAX && &root[js as usize..je as usize] == k {
                            self.ents[j][2] = es;
                            self.ents[j][3] = ee;
                            self.ents[i][0] = u32::MAX;
                            break;
                        }
                    }
                }
            }
        }
        Ok(())
    }

    fn emit_entries<O: Out>(
        &mut self,
        root: &[u8],
        base: usize,
        out: &mut O,
        depth: usize,
    ) -> Result<()> {
        let end = self.ents.len();
        let key_of = |e: [u32; 4]| -> &str {
            // Validated in collect_entries.
            std::str::from_utf8(&root[e[0] as usize..e[1] as usize]).unwrap_or("")
        };
        let mut idx: Vec<(u32, usize)> = Vec::new();
        for i in base..end {
            let e = self.ents[i];
            if e[0] != u32::MAX {
                if let Some(ix) = array_index(key_of(e)) {
                    idx.push((ix, i));
                }
            }
        }
        out.begin_obj();
        if !idx.is_empty() {
            idx.sort_by_key(|p| p.0);
            for &(_, i) in &idx {
                let e = self.ents[i];
                self.entry(root, e, key_of(e), out, depth)?;
            }
        }
        for i in base..end {
            let e = self.ents[i];
            if e[0] == u32::MAX {
                continue;
            }
            let k = key_of(e);
            if !idx.is_empty() && array_index(k).is_some() {
                continue;
            }
            self.entry(root, e, k, out, depth)?;
        }
        out.end_obj();
        Ok(())
    }

    fn entry<O: Out>(
        &mut self,
        root: &[u8],
        e: [u32; 4],
        key: &str,
        out: &mut O,
        depth: usize,
    ) -> Result<()> {
        let at = Src::One(e[2] as usize, e[3] as usize);
        let mut r = Fields::new(root, at);
        let mut val: Option<Occ> = None;
        while let Some(g) = r.next(&self.spans)? {
            if g.number == 2 && g.wt == LEN {
                match &mut val {
                    Some(o) => {
                        o.count += 1;
                        o.last = g;
                    }
                    None => {
                        val = Some(Occ {
                            count: 1,
                            last: g,
                            from: 0,
                        })
                    }
                }
            }
        }
        out.key(key);
        match val {
            None => out.null(),
            Some(o) => {
                let mark = self.spans.len();
                let r = match self.payload(root, at, &o) {
                    Ok(inner) => self.value(root, inner, out, depth),
                    Err(e) => Err(e),
                };
                self.spans.truncate(mark);
                r?;
            }
        }
        Ok(())
    }
}

/// Whether every position in a message of `len` bytes, and its end, fit a
/// `u32` other than `u32::MAX`, which marks a dropped `Struct` entry
/// ([`Dec::ents`]). The wire cannot frame more anyway.
fn positions_fit(len: usize) -> bool {
    len < u32::MAX as usize
}

/// Entries in one `Struct` above which duplicate keys are found by sorting
/// instead of pairwise comparison.
const SORTED_DUP_CHECK_ABOVE: usize = 32;

/// Scratch vector capacity (in entries) kept between decodes; anything a
/// larger event grew beyond this is released when the decode ends.
const SCRATCH_KEEP_ENTRIES: usize = 4096;

thread_local! {
    static DEC: std::cell::RefCell<Dec> = std::cell::RefCell::new(Dec::new());
}

/// Runs `f` with this thread's decoder scratch (a fresh one if re-entered).
pub(super) fn with_dec<T>(f: impl FnOnce(&mut Dec) -> T) -> T {
    DEC.with(|d| match d.try_borrow_mut() {
        Ok(mut d) => {
            let r = f(&mut d);
            d.trim();
            r
        }
        Err(_) => f(&mut Dec::new()),
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    /// A payload of exactly `u32::MAX` bytes is refused: an empty key whose
    /// field ends the message would start at `u32::MAX`, the dropped-entry
    /// mark, and be lost.
    #[test]
    fn a_payload_of_u32_max_bytes_is_too_large() {
        assert!(positions_fit(0));
        assert!(positions_fit(u32::MAX as usize - 1));
        assert!(!positions_fit(u32::MAX as usize));
        assert!(!positions_fit(usize::MAX));
    }

    fn js(x: f64) -> String {
        let mut s = String::new();
        write_js_number(&mut s, x);
        s
    }

    /// The word-at-a-time scan in `TextOut::quoted` escapes exactly what the
    /// byte-at-a-time one does (and JSON's own escaper), wherever the
    /// special byte falls in a word, for every ASCII byte and around
    /// multi-byte characters.
    #[test]
    fn quoted_escapes_what_bytewise_escaping_does() {
        fn bytewise(v: &str) -> String {
            let mut out = String::from("\"");
            for c in v.chars() {
                match c {
                    '"' => out.push_str("\\\""),
                    '\\' => out.push_str("\\\\"),
                    '\n' => out.push_str("\\n"),
                    '\r' => out.push_str("\\r"),
                    '\t' => out.push_str("\\t"),
                    '\u{8}' => out.push_str("\\b"),
                    '\u{c}' => out.push_str("\\f"),
                    c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
                    c => out.push(c),
                }
            }
            out.push('"');
            out
        }
        let quote = |v: &str| {
            let mut s = String::new();
            TextOut::new(&mut s).quoted(v);
            s
        };
        for byte in 0_u8..0x80 {
            let c = char::from(byte);
            for len in 0..20 {
                for at in 0..=len {
                    let mut v: String = "aé€".chars().cycle().take(len).collect();
                    let at = v.char_indices().nth(at).map_or(v.len(), |(i, _)| i);
                    v.insert(at, c);
                    let expected = bytewise(&v);
                    assert_eq!(quote(&v), expected, "{v:?}");
                    assert_eq!(serde_json::to_string(&v).unwrap(), expected, "{v:?}");
                }
            }
        }
        assert_eq!(quote(""), "\"\"");
        assert_eq!(
            quote("plain ascii text, no escapes"),
            "\"plain ascii text, no escapes\""
        );
    }

    fn round_trip(json: &str) -> Value {
        let mut bin = Vec::new();
        crate::proto::encode::encode_json(json.as_bytes(), &mut bin).unwrap();
        match crate::proto::decode::decode_event(&bin).unwrap() {
            crate::proto::decode::Decoded::Event(v) => v,
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn tree_and_text_denote_the_same_js_number_beyond_the_safe_range() {
        // A `Value.number_value` beyond 2^53 is a JS number: the tree holds
        // the rounded f64, the text its JS spelling. Read back, they are the
        // same f64. (`int64` fields are exact and do not take this path.)
        for ts in [
            47_850_746_040_811_521_i64,
            -47_287_796_087_390_210,
            i64::MAX,
            i64::MIN,
        ] {
            let mut text = String::new();
            write_js_number(&mut text, ts as f64);
            let parsed: Value = serde_json::from_str(&text).unwrap();
            let held = js_number(ts as f64);
            assert_eq!(parsed.as_f64(), held.as_f64(), "{ts}");
        }
    }

    /// An `int64` member outside ±(2^53−1) is handed on exactly: enforcement
    /// judges it after middleware, as on SSE (PROC-06). The encoder, which
    /// serves the producer side, refuses it (fuzz `proto_decode`, base64
    /// `GhMKDQgiEIGAgIDwgICA/wESAm0x`: a TEXT_MESSAGE_END with timestamp
    /// -72057563973156863).
    #[test]
    fn an_int64_outside_the_safe_range_is_handed_on_and_the_encoder_refuses_it() {
        use crate::proto::decode::{decode_event, decode_event_json, Decoded};
        let crash: &[u8] = &[
            0x1a, 0x13, 0x0a, 0x0d, 0x08, 0x22, 0x10, 0x81, 0x80, 0x80, 0x80, 0xf0, 0x80, 0x80,
            0x80, 0xff, 0x01, 0x12, 0x02, 0x6d, 0x31,
        ];
        let Ok(Decoded::Event(v)) = decode_event(crash) else {
            panic!("an out-of-range int64 must decode");
        };
        assert_eq!(v["timestamp"], serde_json::json!(-72057563973156863_i64));
        let mut text = String::new();
        decode_event_json(crash, &mut text).expect("text path decodes");
        assert!(text.contains("\"timestamp\":-72057563973156863"), "{text}");
        let mut out = Vec::new();
        let err = crate::proto::encode::encode_value(&v, &mut out).unwrap_err();
        assert_eq!(err.code(), crate::ErrorCode::Encode, "{err}");
        assert!(
            err.message().contains("timestamp") && err.message().contains("safe-integer"),
            "{err}"
        );
        // The bounds themselves are in range on both sides.
        for ts in [MAX_SAFE, -MAX_SAFE] {
            let v = round_trip(&format!(
                r#"{{"type":"TEXT_MESSAGE_END","messageId":"m","timestamp":{ts}}}"#
            ));
            assert_eq!(v["timestamp"], serde_json::json!(ts));
        }
    }

    /// An enum is an `int32` on the wire: a varint beyond 32 bits is read as
    /// its low 32 bits, as protobuf parsers (protobuf-ts `int32()`) read it.
    /// Spelling the untruncated number (`#17439916031`) made a value the
    /// encoder cannot write back (fuzz `proto_decode`: decoded value does
    /// not re-encode, field `op`).
    #[test]
    fn an_enum_varint_beyond_32_bits_is_read_as_int32() {
        use crate::proto::decode::{decode_event, Decoded};
        // STATE_DELTA, delta[0] = {op: 17439916031, path: "/a"}.
        let wide: &[u8] = &[
            0x42, 0x0c, 0x12, 0x0a, 0x08, 0xff, 0xff, 0xff, 0xfb, 0x40, 0x12, 0x02, 0x2f, 0x61,
        ];
        // The same with op = 2^32 + 2, whose low 32 bits are REPLACE.
        let known: &[u8] = &[
            0x42, 0x0c, 0x12, 0x0a, 0x08, 0x82, 0x80, 0x80, 0x80, 0x10, 0x12, 0x02, 0x2f, 0x61,
        ];
        for (bytes, op) in [(wide, "#260046847"), (known, "replace")] {
            let Ok(Decoded::Event(v)) = decode_event(bytes) else {
                panic!("{op}: does not decode");
            };
            assert_eq!(v["delta"][0]["op"], serde_json::json!(op));
            let mut out = Vec::new();
            crate::proto::encode::encode_value(&v, &mut out).unwrap();
            assert_eq!(decode_event(&out).unwrap(), Decoded::Event(v), "{op}");
        }
    }

    #[test]
    fn required_lists_follow_the_schema() {
        // User content is required and may be a list: [] survives.
        let v = round_trip(
            r#"{"type":"MESSAGES_SNAPSHOT","messages":[{"id":"m","role":"user","content":[]}]}"#,
        );
        assert_eq!(v["messages"][0]["content"], serde_json::json!([]));
        // Assistant content is optional: absent stays absent.
        let v = round_trip(
            r#"{"type":"MESSAGES_SNAPSHOT","messages":[{"id":"m","role":"assistant"}]}"#,
        );
        assert!(v["messages"][0].get("content").is_none());
        // A role the schema does not know materialises nothing.
        let v =
            round_trip(r#"{"type":"MESSAGES_SNAPSHOT","messages":[{"id":"m","role":"robot"}]}"#);
        assert!(v["messages"][0].get("content").is_none());
        // Required event lists.
        let v = round_trip(r#"{"type":"STATE_DELTA","delta":[]}"#);
        assert_eq!(v["delta"], serde_json::json!([]));
        let v = round_trip(r#"{"type":"MESSAGES_SNAPSHOT","messages":[]}"#);
        assert_eq!(v["messages"], serde_json::json!([]));
        // An optional list stays absent.
        let v = round_trip(
            r#"{"type":"MESSAGES_SNAPSHOT","messages":[{"id":"m","role":"assistant","toolCalls":[]}]}"#,
        );
        assert!(v["messages"][0].get("toolCalls").is_none());
    }

    /// Security audit 2, I1 (`proto_decode` fuzz crash): a part list whose
    /// only element has an arm this build does not know decoded as
    /// `"content": []`, which re-encodes as no content at all. A list left
    /// empty by dropped elements is absent, as protobuf re-encodes it.
    #[test]
    fn a_list_of_only_dropped_elements_round_trips() {
        let mut b = vec![0x4a, 0x7e];
        b.extend([0x70; 10]);
        b.extend([0x48; 22]);
        b.extend([0x12, 0x48, 0x48, 0x48, 0x48, 0x21, 0x42, 0x00]);
        b.extend([0x48; 86]);
        let decode = |b: &[u8]| match crate::proto::decode::decode_event(b).unwrap() {
            crate::proto::decode::Decoded::Event(v) => v,
            other => panic!("{other:?}"),
        };
        let v = decode(&b);
        assert_eq!(
            v,
            serde_json::json!({"type":"MESSAGES_SNAPSHOT","messages":[{"id":"","role":""}]})
        );
        let mut again = Vec::new();
        crate::proto::encode::encode_value(&v, &mut again).unwrap();
        assert_eq!(decode(&again), v);
        // A required part list still materialises, as `[]`.
        let user = round_trip(
            r#"{"type":"MESSAGES_SNAPSHOT","messages":[{"id":"m","role":"user","content":[]}]}"#,
        );
        assert_eq!(user["messages"][0]["content"], serde_json::json!([]));
    }

    /// The `Limits::max_depth` docs promise 128; pin the ceiling to both the
    /// prose and the pipeline default it mirrors.
    #[test]
    fn the_decode_ceiling_is_128_and_the_default_max_depth() {
        assert_eq!(DECODE_MAX_DEPTH, 128);
        assert_eq!(
            DECODE_MAX_DEPTH,
            crate::pipeline::Limits::default().max_depth
        );
    }

    /// proto-4: one hostile frame must not leave its scratch capacity on the
    /// thread for good.
    #[test]
    fn decoder_scratch_does_not_keep_the_capacity_of_a_huge_frame() {
        std::thread::spawn(|| {
            let members: Vec<String> = (0..20_000).map(|i| format!(r#""k{i}":{i}"#)).collect();
            let json = format!(
                r#"{{"type":"STATE_SNAPSHOT","snapshot":{{{}}}}}"#,
                members.join(",")
            );
            let v = round_trip(&json);
            assert_eq!(v["snapshot"].as_object().map(Map::len), Some(20_000));
            DEC.with(|d| {
                let d = d.borrow();
                assert!(
                    d.ents.capacity() <= SCRATCH_KEEP_ENTRIES,
                    "ents {}",
                    d.ents.capacity()
                );
                assert!(
                    d.sort.capacity() <= SCRATCH_KEEP_ENTRIES,
                    "sort {}",
                    d.sort.capacity()
                );
                assert!(
                    d.spans.capacity() <= SCRATCH_KEEP_ENTRIES,
                    "spans {}",
                    d.spans.capacity()
                );
            });
        })
        .join()
        .unwrap();
    }

    fn ld(field: u32, body: &[u8]) -> Vec<u8> {
        let mut out = vec![(field << 3 | 2) as u8];
        let mut n = body.len();
        while n >= 0x80 {
            out.push((n as u8) | 0x80);
            n >>= 7;
        }
        out.push(n as u8);
        out.extend_from_slice(body);
        out
    }

    fn decode_value(b: &[u8]) -> Value {
        match crate::proto::decode::decode_event(b).unwrap() {
            crate::proto::decode::Decoded::Event(v) => v,
            other => panic!("{other:?}"),
        }
    }

    /// `Struct{key: true}` as a `Value.struct_value` payload.
    fn struct_of(key: &str) -> Vec<u8> {
        let mut entry = ld(1, key.as_bytes());
        entry.extend(ld(2, &[0x20, 0x01]));
        ld(1, &entry)
    }

    /// proto-6: setting another member of a oneof clears the earlier one, so
    /// occurrences of `struct_value` before an intervening `string_value`
    /// must not merge with those after it.
    #[test]
    fn a_oneof_arm_switch_discards_the_earlier_occurrences_of_that_arm() {
        let mut value = ld(5, &struct_of("a"));
        value.extend(ld(3, b"s"));
        value.extend(ld(5, &struct_of("b")));
        value.extend(ld(5, &struct_of("c")));
        let mut custom = ld(2, b"n");
        custom.extend(ld(3, &value));
        let v = decode_value(&ld(11, &custom));
        assert_eq!(v["value"], serde_json::json!({"b": true, "c": true}));
    }

    /// proto-6: the same holds for the `Event` envelope's own arms.
    #[test]
    fn an_envelope_arm_switch_discards_the_earlier_occurrences_of_that_arm() {
        let mut b = ld(2, &[ld(2, b"a"), ld(3, b"x")].concat());
        b.extend(ld(7, &[]));
        b.extend(ld(2, &ld(3, b"d")));
        b.extend(ld(2, &ld(3, b"e")));
        // Last arm wins: TEXT_MESSAGE_CONTENT, merged from the two
        // occurrences after the switch only.
        let v = decode_value(&b);
        assert_eq!(v["type"], "TEXT_MESSAGE_CONTENT");
        assert_eq!(v["messageId"], "");
        assert_eq!(v["delta"], "e");
    }

    #[test]
    fn js_number_format() {
        assert_eq!(js(0.5), "0.5");
        assert_eq!(js(-0.0), "0");
        assert_eq!(js(42.0), "42");
        assert_eq!(js(1e21), "1e+21");
        assert_eq!(js(1e20), "100000000000000000000");
        assert_eq!(js(9223372036854775808.0), "9223372036854776000");
        assert_eq!(js(1e-7), "1e-7");
        assert_eq!(js(0.000001), "0.000001");
        assert_eq!(js(123.456), "123.456");
        assert_eq!(js(-1.5e-10), "-1.5e-10");
        assert_eq!(js(f64::NAN), "null");
        assert_eq!(js(1.7976931348623157e308), "1.7976931348623157e+308");
    }
}
