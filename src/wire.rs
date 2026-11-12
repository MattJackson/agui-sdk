//! Wire formats shared by the client and the server.
//!
//! HTTP bindings: SSE (`text/event-stream`) is mandatory for any HTTP
//! implementation (TRN-07); protobuf (`application/vnd.ag-ui.event+proto`) is
//! optional (TRN-08) and needs feature `proto`. The response `Content-Type`
//! selects the parser, matched exactly (PB-07).
//!
//! Media types are compared as HTTP defines them: `type/subtype` matched
//! ASCII case-insensitively, parameters ignored. Every media-type function here
//! works on borrowed header text and allocates nothing.
//!
//! [`Chunk`] (features `client`, `server` or `proto`) is the crate's own byte
//! buffer type for frames and response bodies.
#![cfg_attr(
    not(any(feature = "client", feature = "server", feature = "proto")),
    doc = "",
    doc = "[`Chunk`]: https://docs.rs/agui-sdk/latest/agui_sdk/wire/struct.Chunk.html"
)]

/// Media type of the SSE binding.
pub const SSE_MEDIA_TYPE: &str = "text/event-stream";

/// Media type of the protobuf binding.
pub const PROTO_MEDIA_TYPE: &str = "application/vnd.ag-ui.event+proto";

/// Media type of the request body (`RunAgentInput` as JSON, SSE-01).
pub const JSON_MEDIA_TYPE: &str = "application/json";

/// A response wire format.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum WireFormat {
    /// Server-Sent Events carrying one JSON event per `data` field.
    Sse,
    /// Length-prefixed protobuf frames.
    Protobuf,
}

impl WireFormat {
    /// The media type.
    #[must_use]
    pub const fn media_type(self) -> &'static str {
        match self {
            WireFormat::Sse => SSE_MEDIA_TYPE,
            WireFormat::Protobuf => PROTO_MEDIA_TYPE,
        }
    }

    /// Selects the format for a response `Content-Type` (parameters ignored,
    /// type compared exactly, PB-07). `None` for anything else.
    ///
    /// "Exactly" means the whole `type/subtype` must match, compared ASCII
    /// case-insensitively as HTTP requires; `text/event-stream-v2` or
    /// `application/x-protobuf` are not accepted.
    #[must_use]
    pub fn from_content_type(content_type: &str) -> Option<Self> {
        let essence = essence(content_type);
        if essence.eq_ignore_ascii_case(SSE_MEDIA_TYPE) {
            Some(WireFormat::Sse)
        } else if essence.eq_ignore_ascii_case(PROTO_MEDIA_TYPE) {
            Some(WireFormat::Protobuf)
        } else {
            None
        }
    }

    /// Server-side negotiation (PB-03..PB-05): protobuf when it is enabled and
    /// `accept` admits its media type with positive quality (wildcards
    /// included), SSE otherwise. Relative qualities are not compared. A
    /// missing header means SSE, and so does an `Accept` that admits neither
    /// type: the binding says a producer MUST answer SSE whenever it does not
    /// answer protobuf, and every client must be prepared to read it, so
    /// there is no `406` branch.
    ///
    /// Whether a type is admitted follows HTTP's precedence: the most
    /// specific matching range decides (`application/vnd.ag-ui.event+proto;q=0`
    /// excludes protobuf even next to `*/*`). An empty header counts as
    /// missing; a list element with an unreadable `q` is ignored.
    #[must_use]
    pub fn negotiate(accept: Option<&str>, protobuf_enabled: bool) -> Self {
        match accept {
            Some(accept) if protobuf_enabled && admits(accept, PROTO_MEDIA_TYPE) => {
                WireFormat::Protobuf
            }
            // A producer without protobuf ignores that type and answers SSE,
            // which every client must be prepared to read (PB-05, PB-06).
            _ => WireFormat::Sse,
        }
    }

    /// A valid client `Accept` value: SSE always, plus protobuf when
    /// enabled; explicit, never a wildcard (SSE-02, PB-01, PB-02, PB-06).
    /// SSE is listed first here; the built-in client lists protobuf first
    /// when it is enabled.
    #[must_use]
    pub const fn client_accept(protobuf_enabled: bool) -> &'static str {
        if protobuf_enabled {
            "text/event-stream, application/vnd.ag-ui.event+proto"
        } else {
            "text/event-stream"
        }
    }
}

/// HTTP optional whitespace.
fn is_ows(c: char) -> bool {
    c == ' ' || c == '\t'
}

/// The `type/subtype` of a media type or range, without parameters.
fn essence(text: &str) -> &str {
    let end = text.find(';').unwrap_or(text.len());
    text[..end].trim_matches(is_ows)
}

/// How specifically `range` matches `media`: 3 exact, 2 `type/*`, 1 `*/*`,
/// 0 not at all.
fn specificity(range: &str, media: &str) -> u8 {
    if range.eq_ignore_ascii_case(media) {
        return 3;
    }
    if range == "*/*" {
        return 1;
    }
    match (range.strip_suffix("/*"), media.split_once('/')) {
        (Some(range_type), Some((media_type, _)))
            if range_type.eq_ignore_ascii_case(media_type) =>
        {
            2
        }
        _ => 0,
    }
}

/// The `q` parameter of one `Accept` element, in thousandths. `Some(1000)`
/// when absent; `None` when present but not a valid HTTP qvalue.
fn quality(params: &str) -> Option<u16> {
    for param in params.split(';') {
        let Some((name, value)) = param.split_once('=') else {
            continue;
        };
        if name.trim_matches(is_ows).eq_ignore_ascii_case("q") {
            return qvalue(value.trim_matches(is_ows));
        }
    }
    Some(1000)
}

/// Parses `0[.ddd]` or `1[.000]` into thousandths.
fn qvalue(text: &str) -> Option<u16> {
    let (int, frac) = match text.split_once('.') {
        Some((int, frac)) => (int, frac),
        None => (text, ""),
    };
    if frac.len() > 3 || !frac.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let mut thousandths: u16 = 0;
    let mut scale: u16 = 100;
    for b in frac.bytes() {
        thousandths += u16::from(b - b'0') * scale;
        scale /= 10;
    }
    match int {
        "0" => Some(thousandths),
        "1" if thousandths == 0 => Some(1000),
        _ => None,
    }
}

/// Whether `accept` admits `media` with positive quality, letting the most
/// specific matching range decide (the highest `q` among equally specific
/// ones).
fn admits(accept: &str, media: &str) -> bool {
    let mut best: (u8, u16) = (0, 0);
    for element in accept.split(',') {
        let (range, params) = match element.split_once(';') {
            Some((range, params)) => (range, params),
            None => (element, ""),
        };
        let rank = specificity(range.trim_matches(is_ows), media);
        if rank == 0 || rank < best.0 {
            continue;
        }
        let Some(q) = quality(params) else {
            continue;
        };
        best = if rank > best.0 {
            (rank, q)
        } else {
            (rank, best.1.max(q))
        };
    }
    best.1 > 0
}

/// An immutable, cheaply cloneable byte buffer: what transports receive, what
/// the decoders slice and what the server's response body yields.
///
/// Cloning and [`slice`](Self::slice) share the allocation (a reference-count
/// increment), so the SSE and protobuf decoders hand out frames without
/// copying. It derefs to `&[u8]`. The backing storage is an implementation
/// detail and is not part of the API: this crate's public signatures carry
/// no third-party type other than `serde_json::Value` and the serde traits
/// (beyond the opt-in `stream` and `axum` feature impls documented at the
/// crate root).
///
/// Present with any of the `client`, `server` or `proto` features.
#[cfg(any(feature = "client", feature = "server", feature = "proto"))]
#[cfg_attr(
    docsrs,
    doc(cfg(any(feature = "client", feature = "server", feature = "proto")))
)]
#[derive(Clone, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Chunk(bytes::Bytes);

#[cfg(any(feature = "client", feature = "server", feature = "proto"))]
impl Chunk {
    /// An empty chunk. No allocation.
    #[must_use]
    pub const fn new() -> Self {
        Chunk(bytes::Bytes::new())
    }

    /// A chunk over static bytes. No allocation.
    #[must_use]
    pub const fn from_static(bytes: &'static [u8]) -> Self {
        Chunk(bytes::Bytes::from_static(bytes))
    }

    /// A chunk holding a copy of `bytes`.
    #[must_use]
    pub fn copy_from_slice(bytes: &[u8]) -> Self {
        Chunk(bytes::Bytes::copy_from_slice(bytes))
    }

    /// Length in bytes.
    #[must_use]
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Whether the chunk is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// A sub-range sharing this chunk's storage. No copy.
    ///
    /// # Panics
    ///
    /// When the range is out of bounds, as slice indexing does.
    #[must_use]
    pub fn slice(&self, range: impl std::ops::RangeBounds<usize>) -> Self {
        Chunk(self.0.slice(range))
    }

    /// The bytes as a `Vec`, copying only when the storage is shared.
    #[must_use]
    pub fn into_vec(self) -> Vec<u8> {
        self.0.into()
    }

    #[allow(dead_code)] // used by the hyper and axum glue
    pub(crate) fn from_bytes(bytes: bytes::Bytes) -> Self {
        Chunk(bytes)
    }

    #[allow(dead_code)] // used by the hyper and axum glue
    pub(crate) fn into_bytes(self) -> bytes::Bytes {
        self.0
    }
}

#[cfg(any(feature = "client", feature = "server", feature = "proto"))]
impl std::ops::Deref for Chunk {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        &self.0
    }
}

#[cfg(any(feature = "client", feature = "server", feature = "proto"))]
impl AsRef<[u8]> for Chunk {
    fn as_ref(&self) -> &[u8] {
        &self.0
    }
}

#[cfg(any(feature = "client", feature = "server", feature = "proto"))]
impl std::fmt::Debug for Chunk {
    /// Length only; payloads can carry secrets.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Chunk({} bytes)", self.0.len())
    }
}

#[cfg(any(feature = "client", feature = "server", feature = "proto"))]
impl From<Vec<u8>> for Chunk {
    /// Takes ownership; no copy.
    fn from(bytes: Vec<u8>) -> Self {
        Chunk(bytes.into())
    }
}

#[cfg(any(feature = "client", feature = "server", feature = "proto"))]
impl From<String> for Chunk {
    /// Takes ownership; no copy.
    fn from(text: String) -> Self {
        Chunk(text.into())
    }
}

#[cfg(any(feature = "client", feature = "server", feature = "proto"))]
impl From<&'static [u8]> for Chunk {
    fn from(bytes: &'static [u8]) -> Self {
        Chunk::from_static(bytes)
    }
}

#[cfg(any(feature = "client", feature = "server", feature = "proto"))]
impl From<&'static str> for Chunk {
    fn from(text: &'static str) -> Self {
        Chunk::from_static(text.as_bytes())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn client_accept_is_built_from_the_media_type_consts() {
        assert_eq!(WireFormat::client_accept(false), SSE_MEDIA_TYPE);
        assert_eq!(
            WireFormat::client_accept(true),
            format!("{SSE_MEDIA_TYPE}, {PROTO_MEDIA_TYPE}")
        );
    }

    /// covers: PB-07
    #[test]
    fn content_type_selects_the_parser_by_exact_type() {
        let from = WireFormat::from_content_type;
        assert_eq!(from("text/event-stream"), Some(WireFormat::Sse));
        assert_eq!(
            from("text/event-stream; charset=utf-8"),
            Some(WireFormat::Sse)
        );
        assert_eq!(from("Text/Event-Stream"), Some(WireFormat::Sse));
        assert_eq!(from(PROTO_MEDIA_TYPE), Some(WireFormat::Protobuf));
        assert_eq!(
            from(" application/vnd.ag-ui.event+proto ;v=1"),
            Some(WireFormat::Protobuf)
        );
        for other in [
            "",
            "application/json",
            "text/event-stream-v2",
            "text/event",
            "application/vnd.ag-ui.event",
            "application/x-protobuf",
            "*/*",
        ] {
            assert_eq!(from(other), None, "{other:?}");
        }
        for format in [WireFormat::Sse, WireFormat::Protobuf] {
            assert_eq!(from(format.media_type()), Some(format));
        }
    }

    /// covers: SSE-02, PB-01, PB-02, PB-06
    #[test]
    fn client_accept_is_explicit_and_always_names_sse() {
        let plain = WireFormat::client_accept(false);
        assert_eq!(plain, SSE_MEDIA_TYPE);
        let both = WireFormat::client_accept(true);
        assert!(!both.contains('*') && !both.contains("q="));
        let names: Vec<&str> = both.split(',').map(essence).collect();
        assert_eq!(names, [SSE_MEDIA_TYPE, PROTO_MEDIA_TYPE]);
        // A server negotiating against what a client sends lands on the
        // format that client can read.
        assert_eq!(WireFormat::negotiate(Some(plain), true), WireFormat::Sse);
        assert_eq!(
            WireFormat::negotiate(Some(both), true),
            WireFormat::Protobuf
        );
        assert_eq!(WireFormat::negotiate(Some(both), false), WireFormat::Sse);
    }

    /// covers: PB-04
    #[test]
    fn protobuf_whenever_admitted_with_positive_quality() {
        let proto = |accept| WireFormat::negotiate(Some(accept), true);
        for accept in [
            PROTO_MEDIA_TYPE,
            "application/vnd.ag-ui.event+proto;q=0.001",
            "*/*",
            "application/*",
            "APPLICATION/*;q=0.5",
            "text/event-stream, */*;q=0.1",
            "text/html , application/vnd.ag-ui.event+proto ; q=1.0",
        ] {
            assert_eq!(proto(accept), WireFormat::Protobuf, "{accept:?}");
        }
    }

    /// covers: PB-03
    #[test]
    fn relative_qualities_are_not_compared() {
        let accept = "text/event-stream;q=1, application/vnd.ag-ui.event+proto;q=0.1";
        assert_eq!(
            WireFormat::negotiate(Some(accept), true),
            WireFormat::Protobuf
        );
    }

    /// covers: PB-05
    #[test]
    fn sse_otherwise_even_when_neither_is_admitted() {
        let neg = WireFormat::negotiate;
        assert_eq!(neg(None, true), WireFormat::Sse);
        assert_eq!(neg(Some(""), true), WireFormat::Sse);
        assert_eq!(neg(Some(" "), true), WireFormat::Sse);
        assert_eq!(neg(Some("text/event-stream"), true), WireFormat::Sse);
        assert_eq!(neg(Some("text/*"), true), WireFormat::Sse);
        assert_eq!(neg(Some("*/*"), false), WireFormat::Sse);
        assert_eq!(neg(Some(PROTO_MEDIA_TYPE), false), WireFormat::Sse);
        // Most specific range wins over a wildcard.
        assert_eq!(
            neg(Some("*/*, application/vnd.ag-ui.event+proto;q=0"), true),
            WireFormat::Sse
        );
        assert_eq!(
            neg(Some("application/*;q=0, text/event-stream"), true),
            WireFormat::Sse
        );
        // Admitting neither is still answered with SSE: the binding leaves
        // no 406 branch ("MUST answer SSE otherwise").
        assert_eq!(neg(Some("application/json"), true), WireFormat::Sse);
        assert_eq!(neg(Some("text/event-stream;q=0"), false), WireFormat::Sse);
        assert_eq!(neg(Some("*/*;q=0"), true), WireFormat::Sse);
        // Admitting protobuf is enough to be served, in SSE when protobuf is
        // off: a client must always be prepared for SSE (PB-06).
        assert_eq!(
            neg(Some("text/event-stream;q=0, */*"), false),
            WireFormat::Sse
        );
        // An unreadable weight does not opt in.
        assert_eq!(
            neg(Some("application/vnd.ag-ui.event+proto;q=2"), true),
            WireFormat::Sse
        );
        assert_eq!(
            neg(
                Some("application/vnd.ag-ui.event+proto;q=abc, text/event-stream"),
                true
            ),
            WireFormat::Sse
        );
    }

    #[test]
    fn qvalues_follow_the_http_grammar() {
        assert_eq!(qvalue("0"), Some(0));
        assert_eq!(qvalue("0.5"), Some(500));
        assert_eq!(qvalue("0.123"), Some(123));
        assert_eq!(qvalue("0."), Some(0));
        assert_eq!(qvalue("1"), Some(1000));
        assert_eq!(qvalue("1.000"), Some(1000));
        for bad in ["", "1.5", "0.1234", "2", "-0", "0.a", ".5", "01"] {
            assert_eq!(qvalue(bad), None, "{bad:?}");
        }
    }
}
