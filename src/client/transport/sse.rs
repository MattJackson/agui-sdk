//! Incremental Server-Sent Events decoder (`transports/http-sse.mdx`).
//!
//! Accepts LF, CR and CRLF line endings (SSE-05, SSE-06), ignores fields other
//! than `data` (SSE-07) and comment lines (SSE-08), joins multi-line `data`
//! per the SSE rules (SSE-04), and yields each event's data. Each data
//! payload must be exactly one JSON event object; that is judged downstream.
//!
//! Zero copy where possible: a single-line `data:` field that lies within one
//! received chunk is returned as a slice of that chunk. Lines split across
//! chunks, and multi-line data, are assembled in a reused buffer. Cost is
//! O(bytes) with one scan per byte.
//!
//! The decoder follows the WHATWG event-stream grammar: one leading UTF-8 BOM
//! is skipped, a line without a colon is a field with an empty value, one
//! space after the colon is removed, field names are case-sensitive, and a
//! blank line dispatches the event only if it carried `data`. Every `data`
//! line is checked to be UTF-8; comments and ignored fields may hold any
//! bytes. `event`, `id` and `retry` are read and dropped: AG-UI
//! has no event names and no resumption (SSE-16).

use std::collections::VecDeque;

use bytes::BytesMut;

use crate::error::{Error, ErrorCode, Result};
use crate::wire::Chunk;

/// Default largest accepted event data: 16 MiB.
pub const DEFAULT_MAX_EVENT_BYTES: usize = crate::pipeline::DEFAULT_MAX_FRAME_BYTES;

/// Bytes a line may carry beyond the event budget: the field name, the colon
/// and the optional space (`data: `), with room for short unknown fields.
const LINE_SLACK: usize = 64;

/// Whether a line starting with `head` could still be a `data` line: its
/// first five bytes are `data:`, or it is shorter and a prefix of that.
fn could_be_data(head: impl Iterator<Item = u8>) -> bool {
    head.take(5)
        .enumerate()
        .all(|(i, byte)| byte == b"data:"[i])
}

/// Appends `bytes`, growing the buffer by doubling as usual but never past
/// `cap` (the longest line accepted), so a line near the cap does not leave
/// twice that capacity behind.
fn extend_within(buf: &mut Vec<u8>, bytes: &[u8], cap: usize) {
    let needed = buf.len() + bytes.len();
    if needed > buf.capacity() {
        let target = needed.max(buf.capacity().saturating_mul(2).min(cap));
        buf.reserve_exact(target - buf.len());
    }
    buf.extend_from_slice(bytes);
}

const BOM: [u8; 3] = [0xEF, 0xBB, 0xBF];

/// How far the decoder is through a leading byte-order mark.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Bom {
    /// This many leading BOM bytes matched so far (0..=2).
    Matching(u8),
    /// Past the start of the stream.
    Done,
}

/// The SSE decoder. No `Default`: a zero limit would reject every event;
/// use [`SseDecoder::new`] with [`DEFAULT_MAX_EVENT_BYTES`] or your budget.
#[derive(Debug)]
pub struct SseDecoder {
    max_event_bytes: usize,
    /// Received chunks not yet fully scanned; the current line starts at
    /// `pos` in the front chunk.
    queue: VecDeque<Chunk>,
    /// Start of the current line in `queue[0]`.
    pos: usize,
    /// Resume point of the terminator search in `queue[0]` (`pos..scan`
    /// holds no terminator), so no byte is scanned twice.
    scan: usize,
    /// Head of a line that started in an earlier chunk.
    pending: Vec<u8>,
    /// The last line ended with CR at the end of a chunk: skip one LF.
    skip_lf: bool,
    bom: Bom,
    /// The event's data when it is one `data` line inside one chunk.
    single: Option<Chunk>,
    /// The event's data otherwise, assembled in place; split off (and its
    /// capacity reclaimed later) when the event is dispatched.
    data: BytesMut,
    /// A `data` field was seen since the last dispatch.
    has_data: bool,
    /// The first error; every later call reports it again.
    failed: Option<(ErrorCode, String)>,
}

impl SseDecoder {
    /// A decoder accepting events up to `max_event_bytes` of data. Held
    /// memory stays bounded by about twice that: the data held plus one
    /// ignored line of up to `max_event_bytes` and a little slack.
    #[must_use]
    pub fn new(max_event_bytes: usize) -> Self {
        SseDecoder {
            max_event_bytes,
            queue: VecDeque::new(),
            pos: 0,
            scan: 0,
            pending: Vec::new(),
            skip_lf: false,
            bom: Bom::Matching(0),
            single: None,
            data: BytesMut::new(),
            has_data: false,
            failed: None,
        }
    }

    /// Adds received bytes.
    pub fn push(&mut self, chunk: Chunk) {
        if chunk.is_empty() || self.failed.is_some() {
            return;
        }
        self.queue.push_back(chunk);
    }

    /// The next complete event's data, if one is buffered.
    ///
    /// # Errors
    ///
    /// [`ErrorCode::SseFraming`] for invalid
    /// UTF-8; [`ErrorCode::FrameTooLarge`]
    /// past the limit (checked as bytes arrive, before buffering more).
    pub fn next_event(&mut self) -> Result<Option<Chunk>> {
        if let Some((code, message)) = &self.failed {
            return Err(Error::new(*code, message.clone()));
        }
        match self.advance() {
            Ok(event) => Ok(event),
            Err((code, message)) => {
                self.poison();
                self.failed = Some((code, message.clone()));
                Err(Error::new(code, message))
            }
        }
    }

    /// Whether an incomplete event is buffered. At end of body, the run was
    /// truncated mid-event. An unterminated last line that cannot be a
    /// `data` line (a comment, an ignored field) is discarded, as WHATWG
    /// discards an incomplete line, and does not count.
    #[must_use]
    pub fn has_partial(&self) -> bool {
        let bom_head = matches!(self.bom, Bom::Matching(n) if n > 0);
        if self.has_data || bom_head {
            return true;
        }
        // Lines not yet scanned (input pushed but not read): judged by
        // `next_event`, so they may hold an event.
        let more_lines = self.queue.len() > 1
            || self.queue.front().is_some_and(|c| {
                memchr::memchr2(b'\n', b'\r', &c[self.scan.min(c.len())..]).is_some()
            });
        if more_lines {
            return true;
        }
        let rest = self
            .queue
            .front()
            .map_or(&[][..], |c| &c[self.pos.min(c.len())..]);
        let mut head = self.pending.iter().chain(rest).copied().peekable();
        head.peek().is_some() && could_be_data(head)
    }

    /// Frees the buffers after an error; input from then on is ignored.
    fn poison(&mut self) {
        self.queue.clear();
        self.pos = 0;
        self.scan = 0;
        self.pending = Vec::new();
        self.single = None;
        self.data = BytesMut::new();
        self.has_data = false;
        self.bom = Bom::Done;
    }

    /// Longest line acceptable now. A line that may be a `data` line gets
    /// the room left in the event budget plus the field-name slack; any
    /// other line (comment, ignored field) is dropped, never joined, so it is
    /// bounded by the whole budget instead.
    fn line_cap(&self, head: impl Iterator<Item = u8>) -> usize {
        if could_be_data(head) {
            let held = self.data_len();
            self.max_event_bytes
                .saturating_sub(held)
                .saturating_add(LINE_SLACK)
        } else {
            self.max_event_bytes.saturating_add(LINE_SLACK)
        }
    }

    fn data_len(&self) -> usize {
        match &self.single {
            Some(chunk) => chunk.len(),
            None => self.data.len(),
        }
    }

    fn too_large(&self) -> (ErrorCode, String) {
        (
            ErrorCode::FrameTooLarge,
            format!(
                "SSE event data exceeds the {}-byte limit",
                self.max_event_bytes
            ),
        )
    }

    /// Scans queued input until an event is complete or input runs out.
    fn advance(&mut self) -> std::result::Result<Option<Chunk>, (ErrorCode, String)> {
        loop {
            let Some(chunk) = self.queue.front() else {
                return Ok(None);
            };
            if self.pos >= chunk.len() {
                self.queue.pop_front();
                self.pos = 0;
                self.scan = 0;
                continue;
            }
            // Second half of a CRLF split across chunks.
            if self.skip_lf {
                self.skip_lf = false;
                if chunk[self.pos] == b'\n' {
                    self.pos += 1;
                    self.scan = self.pos;
                    continue;
                }
            }
            // A leading BOM, possibly split across chunks.
            if let Bom::Matching(n) = self.bom {
                if chunk[self.pos] == BOM[usize::from(n)] {
                    self.pos += 1;
                    self.scan = self.pos;
                    self.bom = if n == 2 {
                        Bom::Done
                    } else {
                        Bom::Matching(n + 1)
                    };
                    continue;
                }
                // Not a BOM after all: the matched bytes are line content.
                self.pending.extend_from_slice(&BOM[..usize::from(n)]);
                self.bom = Bom::Done;
            }

            let cap = self.line_cap(
                self.pending
                    .iter()
                    .copied()
                    .chain(chunk[self.pos..].iter().copied()),
            );
            let have = self.pending.len() + (self.scan - self.pos);
            // Look no further than a line of exactly `cap` bytes plus its
            // terminator, so an endless line is refused in O(cap) work.
            let allowed = cap.saturating_sub(have).saturating_add(1);
            let end = chunk.len().min(self.scan.saturating_add(allowed));
            match memchr::memchr2(b'\n', b'\r', &chunk[self.scan..end]) {
                Some(i) => {
                    let eol = self.scan + i;
                    let mut next = eol + 1;
                    if chunk[eol] == b'\r' {
                        if next < chunk.len() {
                            if chunk[next] == b'\n' {
                                next += 1;
                            }
                        } else {
                            self.skip_lf = true;
                        }
                    }
                    let start = self.pos;
                    self.pos = next;
                    self.scan = next;
                    if self.line(start, eol, cap)? {
                        return Ok(Some(self.dispatch()));
                    }
                }
                None => {
                    self.scan = end;
                    let have = self.pending.len() + (self.scan - self.pos);
                    if have > cap {
                        return Err(self.too_large());
                    }
                    if self.scan == chunk.len() {
                        // The line continues in a later chunk: keep its head.
                        extend_within(&mut self.pending, &chunk[self.pos..], cap);
                        self.queue.pop_front();
                        self.pos = 0;
                        self.scan = 0;
                    }
                }
            }
        }
    }

    /// Applies one complete line: `queue[0][start..eol]`, prefixed by the
    /// pending head when the line began in an earlier chunk. Returns whether
    /// it dispatched an event.
    fn line(
        &mut self,
        start: usize,
        eol: usize,
        cap: usize,
    ) -> std::result::Result<bool, (ErrorCode, String)> {
        let Some(chunk) = self.queue.front() else {
            return Ok(false);
        };
        let whole = self.pending.is_empty();
        if !whole {
            extend_within(&mut self.pending, &chunk[start..eol], cap);
        }
        let line: &[u8] = if whole {
            &chunk[start..eol]
        } else {
            &self.pending
        };
        if line.is_empty() {
            self.pending.clear();
            // Any `data` field dispatches, an empty one included: WHATWG
            // appends the value plus LF, so the buffer is not empty, and the
            // event's data is `""`, a frame the pipeline refuses (SSE-04).
            return Ok(self.has_data);
        }
        if line[0] == b':' {
            self.pending.clear();
            return Ok(false); // comment (SSE-08)
        }
        let (name_end, value_start) = match memchr::memchr(b':', line) {
            Some(colon) if line.get(colon + 1) == Some(&b' ') => (colon, colon + 2),
            Some(colon) => (colon, colon + 1),
            None => (line.len(), line.len()),
        };
        if &line[..name_end] != b"data" {
            // `event`, `id`, `retry` and unknown fields (SSE-07, SSE-16).
            self.pending.clear();
            return Ok(false);
        }
        // Most lines are ASCII, which the word-at-a-time `is_ascii` settles
        // faster than full validation. This is the only full check of the
        // data's UTF-8: enforcement validates the strings it keeps, but
        // passes over stripped members unread.
        if !line.is_ascii() && std::str::from_utf8(line).is_err() {
            return Err((
                ErrorCode::SseFraming,
                "SSE line is not valid UTF-8".to_owned(),
            ));
        }
        let value_len = line.len() - value_start;
        let joined = self.data_len() + usize::from(self.has_data) + value_len;
        if joined > self.max_event_bytes {
            return Err(self.too_large());
        }
        if !self.has_data && whole {
            // The common case: one data line inside one chunk. No copy.
            self.single = Some(chunk.slice(start + value_start..eol));
        } else {
            if let Some(first) = self.single.take() {
                self.data.extend_from_slice(&first);
            }
            if self.has_data {
                self.data.extend_from_slice(b"\n");
            }
            let value = if whole {
                &chunk[start + value_start..eol]
            } else {
                &self.pending[value_start..]
            };
            self.data.extend_from_slice(value);
        }
        self.has_data = true;
        self.pending.clear();
        Ok(false)
    }

    /// Hands out the completed event's data and resets for the next one.
    fn dispatch(&mut self) -> Chunk {
        self.has_data = false;
        match self.single.take() {
            Some(chunk) => chunk,
            None => Chunk::from_bytes(self.data.split().freeze()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decode(chunks: &[&[u8]], max: usize) -> (Vec<String>, Option<ErrorCode>, bool) {
        let mut decoder = SseDecoder::new(max);
        let mut events = Vec::new();
        for c in chunks {
            decoder.push(Chunk::copy_from_slice(c));
            loop {
                match decoder.next_event() {
                    Ok(Some(e)) => events.push(String::from_utf8_lossy(&e).into_owned()),
                    Ok(None) => break,
                    Err(e) => return (events, Some(e.code()), decoder.has_partial()),
                }
            }
        }
        let partial = decoder.has_partial();
        (events, None, partial)
    }

    fn one(input: &[u8]) -> (Vec<String>, Option<ErrorCode>, bool) {
        decode(&[input], DEFAULT_MAX_EVENT_BYTES)
    }

    /// Every split of `input` into two and three pieces, and byte at a time,
    /// decodes exactly as the whole input does.
    fn split_invariant(input: &[u8], max: usize) {
        let whole = decode(&[input], max);
        let bytes: Vec<&[u8]> = input.chunks(1).collect();
        assert_eq!(decode(&bytes, max), whole, "byte at a time: {input:?}");
        for i in 0..=input.len() {
            let (a, b) = input.split_at(i);
            assert_eq!(decode(&[a, b], max), whole, "split at {i}: {input:?}");
            for j in i..=input.len() {
                let (b1, c) = input[i..].split_at(j - i);
                assert_eq!(decode(&[a, b1, c], max), whole, "split {i},{j}");
            }
        }
    }

    #[test]
    fn grammar() {
        let (e, err, partial) = one(b"data: {\"a\":1}\n\n");
        assert_eq!(
            (e, err, partial),
            (vec!["{\"a\":1}".to_owned()], None, false)
        );
        let (e, _, _) = one(b"data: a\ndata:b\ndata\n\n");
        assert_eq!(e, ["a\nb\n"]);
        let (e, _, _) = one(b": c\nevent: x\nid: 1\nretry: 5\nfoo: bar\ndata:  x\n\n");
        assert_eq!(e, [" x"]);
        let (e, _, _) = one(b"event: x\n\ndata: y\n\n");
        assert_eq!(e, ["y"]);
        let (e, _, _) = one(b"Data: x\n\ndata: y\r\rdata: z\r\n\r\n");
        assert_eq!(e, ["y", "z"]);
        let (e, _, _) = one(b"\xEF\xBB\xBFdata: a\n\n\xEF\xBB\xBFdata: b\n\n");
        assert_eq!(e, ["a"]);
    }

    /// Data lines join as WHATWG specifies, including its dispatch step: one
    /// empty `data` field dispatches an event with data `""`, and two join
    /// to a newline.
    ///
    /// covers: SSE-04
    #[test]
    fn an_empty_data_field_dispatches_an_empty_event() {
        for input in [
            &b"data:\n\ndata: {\"a\":1}\n\n"[..],
            b"data\n\ndata: {\"a\":1}\n\n",
            b"data: \r\n\r\ndata: {\"a\":1}\r\n\r\n",
            b"event: x\ndata:\nid: 1\n\ndata: {\"a\":1}\n\n",
        ] {
            let (e, err, partial) = one(input);
            assert_eq!(e, ["", "{\"a\":1}"], "{input:?}");
            assert_eq!((err, partial), (None, false), "{input:?}");
            split_invariant(input, DEFAULT_MAX_EVENT_BYTES);
        }
        let (e, _, _) = one(b"data:\ndata:\n\n");
        assert_eq!(e, ["\n"]);
        let (e, _, partial) = one(b"data:\n\n");
        assert!(e == [""] && !partial);
    }

    #[test]
    fn truncation_and_errors() {
        assert!(one(b"data: a\n").2);
        assert!(one(b"data: a").2);
        assert!(one(b"\xEF\xBB").2);
        assert!(!one(b"data: a\n\n: tail\n\n").2);
        assert!(!one(b"").2);
        assert_eq!(one(b"data: \xFF\n\n").1, Some(ErrorCode::SseFraming));
        assert_eq!(
            decode(&[b"data: 12345\n\n"], 4).1,
            Some(ErrorCode::FrameTooLarge)
        );
        assert_eq!(decode(&[b"data: 1234\n\n"], 4).0, ["1234"]);
        assert_eq!(
            decode(&[b"data: 12\ndata: 12\n\n"], 4).1,
            Some(ErrorCode::FrameTooLarge)
        );
        // An endless line is refused before its end arrives.
        let long = vec![b'x'; 200];
        assert_eq!(decode(&[&long], 4).1, Some(ErrorCode::FrameTooLarge));
    }

    /// An unterminated trailing line that cannot be a `data` line is
    /// discarded at end of body (WHATWG), so it does not truncate the run;
    /// one that may still be `data` does.
    #[test]
    fn an_unterminated_ignored_line_is_not_a_partial_event() {
        for tail in [
            &b": bye"[..],
            b"id: 1",
            b"event: x",
            b"retry",
            b"x",
            b"dat:",
        ] {
            let mut input = b"data: {\"a\":1}\n\n".to_vec();
            input.extend_from_slice(tail);
            let (e, err, partial) = one(&input);
            assert_eq!((e.len(), err, partial), (1, None, false), "{tail:?}");
            split_invariant(&input, DEFAULT_MAX_EVENT_BYTES);
        }
        for tail in [&b"data: {"[..], b"data", b"dat", b"d", b"data:"] {
            let mut input = b"data: {\"a\":1}\n\n".to_vec();
            input.extend_from_slice(tail);
            assert!(one(&input).2, "{tail:?}");
            split_invariant(&input, DEFAULT_MAX_EVENT_BYTES);
        }
    }

    /// A long ignored line arriving in many chunks does not leave the line
    /// buffer at twice the line cap (the documented bound).
    #[test]
    fn the_line_buffer_stays_within_the_line_cap() {
        let max = 1000;
        let mut decoder = SseDecoder::new(max);
        decoder.push(Chunk::copy_from_slice(b": "));
        for _ in 0..(max + LINE_SLACK - 2) {
            decoder.push(Chunk::copy_from_slice(b"x"));
            assert!(matches!(decoder.next_event(), Ok(None)));
        }
        assert!(
            decoder.pending.capacity() <= max + LINE_SLACK,
            "{}",
            decoder.pending.capacity()
        );
        decoder.push(Chunk::copy_from_slice(b"\ndata: 1\n\n"));
        assert!(matches!(decoder.next_event(), Ok(Some(_))));
        assert!(decoder.pending.capacity() <= max + LINE_SLACK);
    }

    /// Only `data` lines must be UTF-8: comments and ignored fields may hold
    /// any bytes (SSE-07, SSE-08).
    #[test]
    fn non_utf8_comments_and_ignored_fields_are_tolerated() {
        let input = b": \xFF\xFE\nevent: \xFF\nid: \xC3\nretry: \x80\ndata: {}\n\n";
        let (e, err, partial) = one(input);
        assert_eq!((e, err, partial), (vec!["{}".to_owned()], None, false));
        split_invariant(input, DEFAULT_MAX_EVENT_BYTES);
        assert_eq!(one(b"data: \xFF\n\n").1, Some(ErrorCode::SseFraming));
    }

    /// Data lines are held to UTF-8 wherever the first non-ASCII byte falls:
    /// valid multi-byte text passes, an invalid or truncated sequence after
    /// any ASCII prefix is refused.
    #[test]
    fn data_lines_are_utf8_after_any_ascii_prefix() {
        for prefix in 0..40 {
            let ascii = "a".repeat(prefix);
            let good = format!("data: {ascii}\u{e9}\u{20ac}\u{1f600}\n\n");
            assert_eq!(one(good.as_bytes()).1, None, "{prefix}");
            for bad in [
                &b"\xFF"[..],
                b"\xC3",
                b"\xE2\x82",
                b"\xED\xA0\x80",
                b"\xC0\xAF",
            ] {
                let mut line = format!("data: {ascii}").into_bytes();
                line.extend_from_slice(bad);
                line.extend_from_slice(b"z\n\n");
                assert_eq!(
                    one(&line).1,
                    Some(ErrorCode::SseFraming),
                    "{prefix} {bad:?}"
                );
            }
        }
    }

    /// Only data lines spend the event budget: a long comment after a data
    /// line near the limit is still dropped, and still bounded.
    #[test]
    fn a_long_ignored_line_does_not_spend_the_event_budget() {
        let mut input = b"data: ".to_vec();
        input.extend(std::iter::repeat(b'a').take(90));
        input.extend_from_slice(b"\n: ");
        input.extend(std::iter::repeat(b'x').take(150));
        input.extend_from_slice(b"\n\n");
        let (e, err, _) = decode(&[&input], 100);
        assert_eq!((e.len(), err), (1, None));
        assert_eq!(e[0].len(), 90);
        // Still bounded: an endless comment, and a data line past the room.
        let mut long = b": ".to_vec();
        long.extend(std::iter::repeat(b'x').take(200));
        assert_eq!(decode(&[&long], 100).1, Some(ErrorCode::FrameTooLarge));
        let mut second = b"data: ".to_vec();
        second.extend(std::iter::repeat(b'a').take(90));
        second.extend_from_slice(b"\ndata: ");
        second.extend(std::iter::repeat(b'b').take(80));
        second.extend_from_slice(b"\n\n");
        assert_eq!(decode(&[&second], 100).1, Some(ErrorCode::FrameTooLarge));
    }

    #[test]
    fn poisoned_after_error() {
        let mut d = SseDecoder::new(64);
        d.push(Chunk::from_static(b"data: \xFF\n\n"));
        assert!(d.next_event().is_err());
        d.push(Chunk::from_static(b"data: ok\n\n"));
        assert!(d.next_event().is_err());
    }

    #[test]
    fn single_line_events_are_slices_of_the_chunk() {
        let input = Chunk::from_static(b"data: abc\n\ndata: de\n\n");
        let mut d = SseDecoder::new(64);
        d.push(input.clone());
        let a = d.next_event().ok().flatten().unwrap_or_default();
        assert_eq!(&*a, b"abc");
        assert_eq!(a.as_ptr(), input[6..].as_ptr());
    }

    #[test]
    fn chunking_never_changes_the_result() {
        for input in [
            &b"data: a\r\n\r\ndata: b\rdata: c\r\r"[..],
            b"\xEF\xBB\xBFdata: x\n\n",
            b"\xEF\xBBdata: x\n\n",
            b": hi\ndata:1\ndata: 2\n\ndata\n\n",
            b"data: \xC3\xA9\n\n",
            b"data: \xC3\n\n",
            b"data: 12345678\n\n",
            b"data: 1234\ndata: 123\n\ndata: x",
        ] {
            split_invariant(input, 8);
            split_invariant(input, DEFAULT_MAX_EVENT_BYTES);
        }
    }
}
