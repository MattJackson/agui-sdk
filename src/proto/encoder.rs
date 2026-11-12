//! JSON event -> protobuf `Event`.
//!
//! Two passes over the same (borrowed) JSON tree: the first measures every
//! length-delimited submessage into a size table, the second writes the bytes
//! into a buffer reserved to the exact final size. Both passes run the same
//! generic code over a [`Sink`], so they cannot disagree.

use super::codec_error::{Error, Result};
use super::decoder::UNKNOWN_ENUM_PREFIX;
use super::json::{self, array_index, JNode, JStr, Kind, Scratch, MAX_DEPTH};
use super::schema::{
    Body, ContentKind, Field, Json, Label, MsgDesc, Ty, BASE_EVENT, EVENT, JSON_PATCH_OPERATION,
    MAX_FIELDS,
};
use super::wire::{put_varint, varint_len, I64, LEN, VARINT};

/// Where encoded bytes go: the measuring pass or the writing pass.
trait Sink {
    fn byte(&mut self, b: u8);
    fn varint(&mut self, v: u64);
    fn fixed64(&mut self, v: u64);
    fn string(&mut self, s: JStr<'_>);
    /// Opens a length-delimited submessage (the tag is already out).
    fn begin(&mut self) -> (usize, usize);
    fn end(&mut self, tok: (usize, usize)) -> Result<()>;

    #[inline]
    fn tag(&mut self, number: u32, wt: u8) {
        self.varint(((number as u64) << 3) | wt as u64);
    }
}

struct Measure<'s> {
    n: usize,
    sizes: &'s mut Vec<usize>,
}

impl Sink for Measure<'_> {
    #[inline]
    fn byte(&mut self, _: u8) {
        self.n += 1;
    }
    #[inline]
    fn varint(&mut self, v: u64) {
        self.n += varint_len(v);
    }
    #[inline]
    fn fixed64(&mut self, _: u64) {
        self.n += 8;
    }
    #[inline]
    fn string(&mut self, s: JStr<'_>) {
        let l = s.len();
        self.n += varint_len(l as u64) + l;
    }
    #[inline]
    fn begin(&mut self) -> (usize, usize) {
        self.sizes.push(0);
        (self.sizes.len() - 1, self.n)
    }
    #[inline]
    fn end(&mut self, (slot, start): (usize, usize)) -> Result<()> {
        let sz = self.n - start;
        if sz > u32::MAX as usize {
            return Err(Error::TooLarge);
        }
        self.sizes[slot] = sz;
        self.n += varint_len(sz as u64);
        Ok(())
    }
}

struct Write<'s> {
    out: &'s mut Vec<u8>,
    sizes: &'s [usize],
    cursor: usize,
}

impl Sink for Write<'_> {
    #[inline]
    fn byte(&mut self, b: u8) {
        self.out.push(b);
    }
    #[inline]
    fn varint(&mut self, v: u64) {
        put_varint(self.out, v);
    }
    #[inline]
    fn fixed64(&mut self, v: u64) {
        self.out.extend_from_slice(&v.to_le_bytes());
    }
    #[inline]
    fn string(&mut self, s: JStr<'_>) {
        match s {
            JStr::Plain(p) => {
                put_varint(self.out, p.len() as u64);
                self.out.extend_from_slice(p.as_bytes());
            }
            JStr::Escaped(_) => {
                put_varint(self.out, s.len() as u64);
                s.write_to(self.out);
            }
        }
    }
    #[inline]
    fn begin(&mut self) -> (usize, usize) {
        let sz = self.sizes[self.cursor];
        self.cursor += 1;
        put_varint(self.out, sz as u64);
        (0, 0)
    }
    #[inline]
    fn end(&mut self, _: (usize, usize)) -> Result<()> {
        Ok(())
    }
}

/// A reusable encoder. Keeps its parse and size buffers between calls while
/// they fit [`SCRATCH_KEEP_BYTES`], so a warm encoder repeating an event of
/// that size allocates nothing beyond growth of the output buffer; a larger
/// event's buffers are released by [`trim`](Self::trim) and regrown.
#[derive(Default, Debug)]
pub(super) struct Encoder {
    scratch: Scratch,
    sizes: Vec<usize>,
    order: Vec<u32>,
}

/// Scratch entries (nodes, sizes, positions) each buffer is cut back to when
/// the scratch outgrows [`SCRATCH_KEEP_BYTES`].
pub(super) const SCRATCH_KEEP_ENTRIES: usize = 4096;

/// Scratch capacity, in bytes over all the encoder's buffers, kept between
/// encodes: the same bound as the per-thread text and string buffers
/// (`encode::KEEP_BYTES`). Below it a warm encoder reuses its buffers, so
/// repeating an event of that size allocates nothing; above it the capacity
/// one huge event grew the buffers to is released when the encode ends
/// (proto-17), and every buffer goes back to [`SCRATCH_KEEP_ENTRIES`].
pub(super) const SCRATCH_KEEP_BYTES: usize = 256 * 1024;

impl Encoder {
    /// A new encoder.
    pub(super) fn new() -> Self {
        Self::default()
    }

    /// Clears the scratch and, if it holds more than [`SCRATCH_KEEP_BYTES`],
    /// releases the capacity a large event grew it to.
    pub(super) fn trim(&mut self) {
        self.scratch.clear();
        self.sizes.clear();
        self.order.clear();
        if self.retained_bytes() > SCRATCH_KEEP_BYTES {
            self.scratch.shrink_to(SCRATCH_KEEP_ENTRIES);
            self.sizes.shrink_to(SCRATCH_KEEP_ENTRIES);
            self.order.shrink_to(SCRATCH_KEEP_ENTRIES);
        }
    }

    /// The scratch capacity held, in bytes.
    pub(super) fn retained_bytes(&self) -> usize {
        self.scratch.capacity_bytes()
            + self.sizes.capacity() * std::mem::size_of::<usize>()
            + self.order.capacity() * std::mem::size_of::<u32>()
    }

    /// The largest scratch buffer capacity, in entries.
    #[cfg(test)]
    pub(super) fn max_capacity(&self) -> usize {
        self.scratch
            .max_capacity()
            .max(self.sizes.capacity())
            .max(self.order.capacity())
    }

    /// Encodes one event given as JSON text, appending the `Event` message to
    /// `out`. Member order in open payloads is taken from the text.
    pub(super) fn encode_into(&mut self, event_json: &str, out: &mut Vec<u8>) -> Result<()> {
        let Encoder {
            scratch,
            sizes,
            order,
        } = self;
        let tree = json::parse(event_json, scratch)?;
        run(tree.root(), sizes, order, out)
    }

    /// Encodes a typed value through [`json::build`]: the tree the JSON text
    /// would parse to, built without the text. `None` when the builder
    /// declines (the caller then takes the text path).
    pub(super) fn encode_typed<T: serde::Serialize + ?Sized>(
        &mut self,
        value: &T,
        text: &mut String,
        out: &mut Vec<u8>,
    ) -> Option<Result<()>> {
        let Encoder {
            scratch,
            sizes,
            order,
        } = self;
        let tree = json::build(value, text, scratch)?;
        Some(run(tree.root(), sizes, order, out))
    }

    /// Encodes one event given as a `serde_json::Value`, appending to `out`.
    pub(super) fn encode_value_into(
        &mut self,
        event: &serde_json::Value,
        out: &mut Vec<u8>,
    ) -> Result<()> {
        run(event, &mut self.sizes, &mut self.order, out)
    }
}

fn run<'a, N: JNode<'a>>(
    root: N,
    sizes: &mut Vec<usize>,
    order: &mut Vec<u32>,
    out: &mut Vec<u8>,
) -> Result<()> {
    sizes.clear();
    order.clear();
    let mut m = Measure { n: 0, sizes };
    event(root, &mut m, order)?;
    let total = m.n;
    out.reserve(total);
    let mut w = Write {
        out,
        sizes,
        cursor: 0,
    };
    event(root, &mut w, order)
}

fn mismatch<'a, N: JNode<'a>>(f: &Field, expected: &'static str, n: N) -> Error {
    Error::TypeMismatch {
        field: f.proto,
        expected,
        found: n.kind().name(),
    }
}

/// The last `type` member of an object, if any.
fn type_member<'a, N: JNode<'a>>(obj: N) -> Option<N> {
    let mut found = None;
    for (k, v) in obj.members() {
        if k.eq_str("type") {
            found = Some(v);
        }
    }
    found
}

fn event<'a, N: JNode<'a>, S: Sink>(root: N, s: &mut S, order: &mut Vec<u32>) -> Result<()> {
    if root.kind() != Kind::Obj {
        return Err(Error::NotAnObject);
    }
    let t = match type_member(root) {
        Some(t) if t.kind() == Kind::Str => t.as_str(),
        _ => return Err(Error::MissingType),
    };
    let Body::Union { arms, .. } = EVENT.body else {
        unreachable!("Event is a union")
    };
    let arm = arms
        .iter()
        .find(|a| t.eq_str(a.json))
        .ok_or_else(|| Error::UnknownEventType(t.cow().into_owned()))?;
    s.tag(arm.number, LEN);
    let tok = s.begin();
    fields(arm.msg, root, s, order, 1, arm.event_type)?;
    s.end(tok)
}

/// Encodes a message body from a JSON object. `event_type` is used for the
/// `BaseEvent.type` of an event's `base_event`.
fn fields<'a, N: JNode<'a>, S: Sink>(
    desc: &MsgDesc,
    obj: N,
    s: &mut S,
    order: &mut Vec<u32>,
    depth: usize,
    event_type: i32,
) -> Result<()> {
    if depth > MAX_DEPTH {
        return Err(Error::DepthLimit);
    }
    let fs = desc.fields();
    let mut slots: [Option<N>; MAX_FIELDS] = [None; MAX_FIELDS];
    let mut base: [Option<N>; MAX_FIELDS] = [None; MAX_FIELDS];
    let mut outcome: Option<N> = None;
    let has_base = matches!(fs.first(), Some(f) if f.json == Json::Base);

    for (k, v) in obj.members() {
        let mut matched = false;
        for (i, f) in fs.iter().enumerate() {
            match f.json {
                Json::Key(name) if k.eq_str(name) => {
                    slots[i] = Some(v);
                    matched = true;
                    break;
                }
                Json::Content(_) if k.eq_str("content") => {
                    // Last `content` wins: clear its siblings, then keep the
                    // one matching this value's kind.
                    for (j, g) in fs.iter().enumerate() {
                        if matches!(g.json, Json::Content(_)) {
                            slots[j] = None;
                        }
                    }
                    let want = match v.kind() {
                        Kind::Str => Some(ContentKind::Text),
                        Kind::Arr => Some(ContentKind::Parts),
                        Kind::Obj => Some(ContentKind::Object),
                        Kind::Null => None,
                        _ => return Err(mismatch(f, "string, array or object", v)),
                    };
                    if let Some(want) = want {
                        match fs.iter().position(|g| g.json == Json::Content(want)) {
                            Some(j) => slots[j] = Some(v),
                            None => return Err(mismatch(f, "string or array", v)),
                        }
                    }
                    matched = true;
                    break;
                }
                Json::OutcomeType if k.eq_str("outcome") => {
                    outcome = Some(v);
                    matched = true;
                    break;
                }
                _ => {}
            }
        }
        if !matched && has_base {
            for (i, f) in BASE_EVENT.fields().iter().enumerate() {
                if let Json::Key(name) = f.json {
                    if k.eq_str(name) {
                        base[i] = Some(v);
                        break;
                    }
                }
            }
        }
    }

    if let Some(o) = outcome {
        match o.kind() {
            Kind::Null => {}
            Kind::Obj => {
                for (k, v) in o.members() {
                    for (i, f) in fs.iter().enumerate() {
                        let hit = match f.json {
                            Json::OutcomeType => k.eq_str("type"),
                            Json::OutcomeKey(name) => k.eq_str(name),
                            _ => false,
                        };
                        if hit {
                            slots[i] = Some(v);
                            break;
                        }
                    }
                }
            }
            _ => {
                return Err(Error::TypeMismatch {
                    field: "outcome",
                    expected: "object",
                    found: o.kind().name(),
                });
            }
        }
    }

    if std::ptr::eq(desc, &JSON_PATCH_OPERATION) {
        undefined_from(fs, &mut slots);
    }

    for (i, f) in fs.iter().enumerate() {
        if f.json == Json::Base {
            s.tag(f.number, LEN);
            let tok = s.begin();
            for (j, bf) in BASE_EVENT.fields().iter().enumerate() {
                if bf.json == Json::EventType {
                    if event_type != 0 {
                        s.tag(bf.number, VARINT);
                        s.varint(event_type as u64);
                    }
                } else if let Some(v) = base[j] {
                    field(bf, v, s, order, depth)?;
                }
            }
            s.end(tok)?;
        } else if let Some(v) = slots[i] {
            field(f, v, s, order, depth)?;
        }
    }
    Ok(())
}

/// RFC 6902 defines `from` only for `move` and `copy`. On any other op (or
/// one whose `op` is not a string, which its own field reports) a `from`
/// that is not a string is an extension member, which protobuf does not
/// carry (PB-16, SNAP-08); a string one still travels as field 3.
fn undefined_from<'a, N: JNode<'a>>(fs: &[Field], slots: &mut [Option<N>; MAX_FIELDS]) {
    let slot = |name: &'static str| fs.iter().position(|f| f.json == Json::Key(name));
    let (Some(op), Some(from)) = (slot("op"), slot("from")) else {
        return;
    };
    let defined = matches!(slots[op], Some(v) if v.kind() == Kind::Str
        && (v.as_str().eq_str("move") || v.as_str().eq_str("copy")));
    if !defined && matches!(slots[from], Some(v) if v.kind() != Kind::Str) {
        slots[from] = None;
    }
}

fn field<'a, N: JNode<'a>, S: Sink>(
    f: &Field,
    v: N,
    s: &mut S,
    order: &mut Vec<u32>,
    depth: usize,
) -> Result<()> {
    if f.label == Label::Repeated {
        match v.kind() {
            Kind::Null => Ok(()),
            Kind::Arr => {
                if depth + 1 > MAX_DEPTH {
                    return Err(Error::DepthLimit);
                }
                for e in v.elements() {
                    if e.kind() == Kind::Null {
                        return Err(mismatch(f, "non-null element", e));
                    }
                    single(f, e, s, order, depth + 1)?;
                }
                Ok(())
            }
            _ => Err(mismatch(f, "array", v)),
        }
    } else {
        single(f, v, s, order, depth)
    }
}

/// Converts a JSON number to an `int64` with JS safe-integer semantics.
fn safe_int<'a, N: JNode<'a>>(f: &Field, v: N) -> Result<i64> {
    const MAX_SAFE: f64 = super::decoder::MAX_SAFE as f64;
    let x = v.as_f64();
    if x.is_finite() && x.fract() == 0.0 && x.abs() <= MAX_SAFE {
        Ok(x as i64)
    } else {
        Err(Error::IntegerOutOfRange { field: f.proto })
    }
}

fn single<'a, N: JNode<'a>, S: Sink>(
    f: &Field,
    v: N,
    s: &mut S,
    order: &mut Vec<u32>,
    depth: usize,
) -> Result<()> {
    let k = v.kind();
    if k == Kind::Null && !matches!(f.ty, Ty::Value) {
        // Absent-not-null: a null member is treated as absent.
        return Ok(());
    }
    match f.ty {
        Ty::String => {
            if k != Kind::Str {
                return Err(mismatch(f, "string", v));
            }
            let st = v.as_str();
            if f.label == Label::Implicit && st.len() == 0 {
                return Ok(());
            }
            s.tag(f.number, LEN);
            s.string(st);
        }
        Ty::Bool => {
            if k != Kind::Bool {
                return Err(mismatch(f, "boolean", v));
            }
            let b = v.as_bool();
            if f.label == Label::Implicit && !b {
                return Ok(());
            }
            s.tag(f.number, VARINT);
            s.byte(b as u8);
        }
        Ty::Int64 => {
            if k != Kind::Num {
                return Err(mismatch(f, "number", v));
            }
            let x = safe_int(f, v)?;
            if f.label == Label::Implicit && x == 0 {
                return Ok(());
            }
            s.tag(f.number, VARINT);
            s.varint(x as u64);
        }
        Ty::Enum(e) => {
            if k != Kind::Str {
                return Err(mismatch(f, "string", v));
            }
            let st = v.as_str();
            // An escaped value is expanded on the stack when it fits, as every
            // spelling and `#<i32>` does; a longer one is copied to the heap.
            let mut buf = [0u8; 32];
            let owned;
            let text: &str = match st.in_buf(&mut buf) {
                Some(t) => t,
                None => {
                    owned = st.cow();
                    &owned
                }
            };
            // A known spelling, or the decoder's `#<number>` for a value this
            // build predates (so a decoded frame re-encodes unchanged).
            let number = match e.by_json(text) {
                Some(ev) => ev.number,
                None => text
                    .strip_prefix(UNKNOWN_ENUM_PREFIX)
                    .filter(|d| !d.starts_with('+'))
                    .and_then(|d| d.parse::<i32>().ok())
                    .filter(|&n| e.by_number(n).is_none())
                    .ok_or_else(|| Error::UnknownEnumValue {
                        field: f.proto,
                        value: text.to_owned(),
                    })?,
            };
            if f.label == Label::Implicit && number == 0 {
                return Ok(());
            }
            s.tag(f.number, VARINT);
            s.varint(number as i64 as u64);
        }
        Ty::Message(m) => {
            if k != Kind::Obj {
                return Err(mismatch(f, "object", v));
            }
            s.tag(f.number, LEN);
            let tok = s.begin();
            message(m, v, s, order, depth + 1)?;
            s.end(tok)?;
        }
        Ty::Struct => {
            if k != Kind::Obj {
                return Err(mismatch(f, "object", v));
            }
            s.tag(f.number, LEN);
            let tok = s.begin();
            structure(v, s, order, depth + 1)?;
            s.end(tok)?;
        }
        Ty::Value => {
            s.tag(f.number, LEN);
            let tok = s.begin();
            value(v, s, order, depth)?;
            s.end(tok)?;
        }
    }
    Ok(())
}

fn message<'a, N: JNode<'a>, S: Sink>(
    m: &MsgDesc,
    v: N,
    s: &mut S,
    order: &mut Vec<u32>,
    depth: usize,
) -> Result<()> {
    match m.body {
        Body::Fields(_) => fields(m, v, s, order, depth, 0),
        Body::Union { arms, .. } => {
            let t = type_member(v)
                .filter(|t| t.kind() == Kind::Str)
                .map(|t| t.as_str());
            let arm = t.and_then(|t| arms.iter().find(|a| t.eq_str(a.json)));
            let Some(arm) = arm else {
                return Err(Error::UnknownVariant {
                    message: m.name,
                    value: t.map(|t| t.cow().into_owned()).unwrap_or_default(),
                });
            };
            s.tag(arm.number, LEN);
            let tok = s.begin();
            fields(arm.msg, v, s, order, depth, 0)?;
            s.end(tok)
        }
    }
}

/// Encodes the body of a `google.protobuf.Value`.
fn value<'a, N: JNode<'a>, S: Sink>(
    v: N,
    s: &mut S,
    order: &mut Vec<u32>,
    depth: usize,
) -> Result<()> {
    match v.kind() {
        Kind::Null => {
            s.tag(1, VARINT);
            s.byte(0);
        }
        Kind::Num => {
            s.tag(2, I64);
            s.fixed64(v.as_f64().to_bits());
        }
        Kind::Str => {
            s.tag(3, LEN);
            s.string(v.as_str());
        }
        Kind::Bool => {
            s.tag(4, VARINT);
            s.byte(v.as_bool() as u8);
        }
        Kind::Obj => {
            s.tag(5, LEN);
            let tok = s.begin();
            structure(v, s, order, depth + 1)?;
            s.end(tok)?;
        }
        Kind::Arr => {
            if depth + 1 > MAX_DEPTH {
                return Err(Error::DepthLimit);
            }
            s.tag(6, LEN);
            let tok = s.begin();
            for e in v.elements() {
                s.tag(1, LEN);
                let t = s.begin();
                value(e, s, order, depth + 1)?;
                s.end(t)?;
            }
            s.end(tok)?;
        }
    }
    Ok(())
}

fn entry<'a, N: JNode<'a>, S: Sink>(
    k: JStr<'_>,
    v: N,
    s: &mut S,
    order: &mut Vec<u32>,
    depth: usize,
) -> Result<()> {
    s.tag(1, LEN);
    let tok = s.begin();
    s.tag(1, LEN);
    s.string(k);
    s.tag(2, LEN);
    let t = s.begin();
    value(v, s, order, depth)?;
    s.end(t)?;
    s.end(tok)
}

fn key_eq(a: JStr<'_>, b: JStr<'_>) -> bool {
    match (a, b) {
        (JStr::Plain(x), JStr::Plain(y)) => x == y,
        _ => a.bytes().eq(b.bytes()),
    }
}

fn key_index(k: JStr<'_>) -> Option<u32> {
    match k {
        JStr::Plain(p) => array_index(p),
        // An index is at most 10 digits; a longer value is not one.
        JStr::Escaped(_) => array_index(k.in_buf(&mut [0u8; 10])?),
    }
}

/// `ix` in canonical decimal, the key it is the array index of.
fn index_key(ix: u32, buf: &mut [u8; 10]) -> &str {
    let mut at = buf.len();
    let mut v = ix;
    loop {
        at -= 1;
        buf[at] = b'0' + (v % 10) as u8;
        v /= 10;
        if v == 0 {
            break;
        }
    }
    std::str::from_utf8(&buf[at..]).unwrap_or("")
}

/// Encodes the body of a `google.protobuf.Struct`, members in the order a JS
/// object would enumerate them after `JSON.parse`: array-index keys first,
/// ascending, then the rest in insertion order; a repeated key keeps its
/// first position and its last value.
fn structure<'a, N: JNode<'a>, S: Sink>(
    obj: N,
    s: &mut S,
    order: &mut Vec<u32>,
    depth: usize,
) -> Result<()> {
    if depth > MAX_DEPTH {
        return Err(Error::DepthLimit);
    }
    let n = obj.member_count();
    let mut plain = true;
    let mut any_index = false;
    for (k, _) in obj.members() {
        if key_index(k).is_some() {
            any_index = true;
            plain = false;
            break;
        }
    }
    if plain && N::MAY_HAVE_DUPLICATE_KEYS && n > 1 {
        plain = !has_duplicates(obj, n, order);
    }
    if plain {
        for (k, v) in obj.members() {
            entry(k, v, s, order, depth)?;
        }
        return Ok(());
    }
    if !N::MAY_HAVE_DUPLICATE_KEYS {
        // A map cannot repeat a key, and positional access into it is not
        // O(1): park the index keys' values in `order` (above whatever
        // enclosing structs have parked there), ascending, and look each up
        // by key; then walk the members for the rest.
        let base = order.len();
        order.extend(obj.members().filter_map(|(k, _)| key_index(k)));
        order[base..].sort_unstable();
        let end = order.len();
        for at in base..end {
            let mut buf = [0u8; 10];
            let k = index_key(order[at], &mut buf);
            if let Some(v) = obj.get(k) {
                entry(JStr::Plain(k), v, s, order, depth)?;
            }
        }
        order.truncate(base);
        for (k, v) in obj.members() {
            if key_index(k).is_none() {
                entry(k, v, s, order, depth)?;
            }
        }
        return Ok(());
    }

    // Slow path: build (key position, value position) pairs in `order`,
    // above whatever enclosing structs have parked there.
    let base = order.len();
    if n <= SORTED_DEDUP_ABOVE {
        for i in 0..n {
            let (k, _) = obj.member(i);
            let mut dup = false;
            let mut j = base;
            while j < order.len() {
                if key_eq(obj.member(order[j] as usize).0, k) {
                    order[j + 1] = i as u32;
                    dup = true;
                    break;
                }
                j += 2;
            }
            if !dup {
                order.push(i as u32);
                order.push(i as u32);
            }
        }
    } else {
        // Sort the member indices by key (ties by position, so equal keys
        // stay in insertion order) in `order[base..base + n]`; each run of
        // equal keys keeps its first position and takes its last value,
        // recorded at the first position in `order[base + n..base + 2n]`.
        // Walking that table in position order leaves the pairs in
        // insertion order, copied down to `base`.
        order.extend(0..n as u32);
        order[base..].sort_unstable_by(|&a, &b| {
            key_cmp(obj.member(a as usize).0, obj.member(b as usize).0).then(a.cmp(&b))
        });
        order.resize(base + 2 * n, u32::MAX);
        let mut g = 0;
        while g < n {
            let first = order[base + g];
            let kf = obj.member(first as usize).0;
            let mut h = g + 1;
            while h < n && key_eq(obj.member(order[base + h] as usize).0, kf) {
                h += 1;
            }
            order[base + n + first as usize] = order[base + h - 1];
            g = h;
        }
        let mut w = base;
        for i in 0..n {
            let last = order[base + n + i];
            if last != u32::MAX {
                order[w] = i as u32;
                order[w + 1] = last;
                w += 2;
            }
        }
        order.truncate(w);
    }
    let end = order.len();
    if any_index {
        // The pairs whose key is an index, by offset, parked above `end` and
        // sorted by that index (keys are distinct here, so indices are too).
        let mut j = base;
        while j < end {
            if key_index(obj.member(order[j] as usize).0).is_some() {
                order.push(j as u32);
            }
            j += 2;
        }
        let (pairs, idx) = order.split_at_mut(end);
        idx.sort_unstable_by_key(|&j| key_index(obj.member(pairs[j as usize] as usize).0));
        let tail = order.len();
        for at in end..tail {
            let j = order[at] as usize;
            let (kp, vp) = (order[j], order[j + 1]);
            entry(
                obj.member(kp as usize).0,
                obj.member(vp as usize).1,
                s,
                order,
                depth,
            )?;
        }
        order.truncate(end);
    }
    let mut j = base;
    while j < end {
        let (kp, vp) = (order[j], order[j + 1]);
        let k = obj.member(kp as usize).0;
        if !(any_index && key_index(k).is_some()) {
            entry(k, obj.member(vp as usize).1, s, order, depth)?;
        }
        j += 2;
    }
    order.truncate(base);
    Ok(())
}

/// Members in one `Struct` above which duplicate keys are found by sorting
/// rather than pairwise comparison.
const SORTED_DEDUP_ABOVE: usize = 16;

fn key_cmp(a: JStr<'_>, b: JStr<'_>) -> std::cmp::Ordering {
    match (a, b) {
        (JStr::Plain(x), JStr::Plain(y)) => x.cmp(y),
        // `str` orders by its UTF-8 bytes.
        _ => a.bytes().cmp(b.bytes()),
    }
}

/// Whether an object repeats a key. Small objects compare pairwise; larger
/// ones sort member indices in the shared scratch (no allocation once warm).
fn has_duplicates<'a, N: JNode<'a>>(obj: N, n: usize, order: &mut Vec<u32>) -> bool {
    if n <= SORTED_DEDUP_ABOVE {
        for i in 1..n {
            let ki = obj.member(i).0;
            for j in 0..i {
                if key_eq(obj.member(j).0, ki) {
                    return true;
                }
            }
        }
        false
    } else {
        let base = order.len();
        order.extend(0..n as u32);
        order[base..]
            .sort_unstable_by(|&a, &b| key_cmp(obj.member(a as usize).0, obj.member(b as usize).0));
        let dup = order[base..]
            .windows(2)
            .any(|w| key_eq(obj.member(w[0] as usize).0, obj.member(w[1] as usize).0));
        order.truncate(base);
        dup
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::super::json::{parse, TNode};
    use super::*;
    use std::cell::Cell;

    /// A `TNode` that counts positional member accesses, the unit of work of
    /// the duplicate-key handling.
    #[derive(Clone, Copy)]
    struct Counting<'a> {
        n: TNode<'a>,
        calls: &'a Cell<u64>,
    }

    impl<'a> JNode<'a> for Counting<'a> {
        const MAY_HAVE_DUPLICATE_KEYS: bool = true;
        fn kind(self) -> Kind {
            self.n.kind()
        }
        fn as_bool(self) -> bool {
            self.n.as_bool()
        }
        fn as_f64(self) -> f64 {
            self.n.as_f64()
        }
        fn as_str(self) -> JStr<'a> {
            self.n.as_str()
        }
        fn elements(self) -> impl Iterator<Item = Self> {
            let calls = self.calls;
            self.n.elements().map(move |n| Counting { n, calls })
        }
        fn members(self) -> impl Iterator<Item = (JStr<'a>, Self)> {
            let calls = self.calls;
            self.n
                .members()
                .map(move |(k, n)| (k, Counting { n, calls }))
        }
        fn member_count(self) -> usize {
            self.n.member_count()
        }
        fn member(self, i: usize) -> (JStr<'a>, Self) {
            self.calls.set(self.calls.get() + 1);
            let (k, n) = self.n.member(i);
            (
                k,
                Counting {
                    n,
                    calls: self.calls,
                },
            )
        }
    }

    fn snapshot_of(members: &[String]) -> String {
        format!(
            r#"{{"type":"STATE_SNAPSHOT","snapshot":{{{}}}}}"#,
            members.join(",")
        )
    }

    /// Positional member reads needed to encode `json`.
    fn member_reads(json: &str) -> u64 {
        let calls = Cell::new(0);
        let mut scratch = Scratch::default();
        let tree = parse(json, &mut scratch).unwrap();
        let (mut sizes, mut order, mut out) = (Vec::new(), Vec::new(), Vec::new());
        run(
            Counting {
                n: tree.root(),
                calls: &calls,
            },
            &mut sizes,
            &mut order,
            &mut out,
        )
        .unwrap();
        calls.get()
    }

    const N: usize = 4096;
    /// Two sorts' worth of comparisons (about 4 n log2 n member reads here),
    /// far below the n^2/2 (8.4M) of a pairwise scan.
    const BOUND: u64 = (N as u64) * 128;

    /// proto-2: array-index keys must not make the struct encode quadratic.
    #[test]
    fn index_keyed_struct_is_encoded_in_n_log_n() {
        let members: Vec<String> = (0..N).map(|i| format!(r#""{i}":{i}"#)).collect();
        let reads = member_reads(&snapshot_of(&members));
        assert!(reads <= BOUND, "{reads} member reads for {N} index keys");
    }

    /// proto-2: one repeated key among many must not make it quadratic either.
    #[test]
    fn one_duplicate_among_many_keys_is_encoded_in_n_log_n() {
        let mut members: Vec<String> = (0..N).map(|i| format!(r#""k{i}":{i}"#)).collect();
        members.push(r#""k0":"again""#.to_owned());
        let reads = member_reads(&snapshot_of(&members));
        assert!(
            reads <= BOUND,
            "{reads} member reads for {N} keys + 1 repeat"
        );
    }

    fn encoded(json: &str) -> Vec<u8> {
        let mut out = Vec::new();
        crate::proto::encode::encode_json(json.as_bytes(), &mut out).unwrap();
        out
    }

    /// proto-2: the member order is unchanged. Index-only keys take the
    /// separate `serde_json::Value` path, an independent oracle.
    #[test]
    fn index_keys_ascend_first_as_the_value_path_orders_them() {
        // Past the pairwise threshold, in scrambled order.
        let members: Vec<String> = (0..40)
            .map(|i| format!(r#""{}":{i}"#, (i * 7) % 40))
            .collect();
        let text = snapshot_of(&members);
        let value: serde_json::Value = serde_json::from_str(&text).unwrap();
        let mut by_value = Vec::new();
        crate::proto::encode::encode_value(&value, &mut by_value).unwrap();
        assert_eq!(encoded(&text), by_value);
    }

    /// proto-2: a repeated key keeps its first position and takes its last
    /// value, among enough keys to take the sorted branch.
    #[test]
    fn a_repeated_key_keeps_its_first_position_and_last_value() {
        let mut members: Vec<String> = (0..40).map(|i| format!(r#""k{i}":{i}"#)).collect();
        members.insert(5, r#""7":"idx""#.to_owned());
        members.push(r#""k0":"last""#.to_owned());
        members.push(r#""7":"idx2""#.to_owned());
        let decoded = |bytes: &[u8]| {
            let mut s = String::new();
            crate::proto::decode::decode_event_json(bytes, &mut s).unwrap();
            s
        };
        let mut want: Vec<String> = vec![r#""7":"idx2""#.to_owned(), r#""k0":"last""#.to_owned()];
        want.extend((1..40).map(|i| format!(r#""k{i}":{i}"#)));
        assert_eq!(
            decoded(&encoded(&snapshot_of(&members))),
            snapshot_of(&want)
        );
    }
}
