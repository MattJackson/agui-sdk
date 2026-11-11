//! SSE encoding: `data: <one JSON event>\n\n`, LF line endings only (SSE-04,
//! SSE-05), keep-alives as `: keep-alive\n\n` comments (SSE-08, SSE-09).
//!
//! Each event is serialized straight into the caller's buffer after the
//! `data: ` prefix, so encoding allocates nothing once the buffer has grown
//! to the largest event. `serde_json` escapes every line break inside a JSON
//! string, so compact JSON has none and the payload is one `data:` line. The
//! encoder does not rely on that: should the serialized text ever hold a CR
//! or LF, the payload is split into one `data:` line per line, which an SSE
//! parser joins back with `\n` (SSE-04), and no CR is ever written (SSE-05).

use crate::error::{Error, ErrorCode, Result};
use crate::event::Event;

/// Field prefix of a data line.
const DATA: &[u8] = b"data: ";

/// The keep-alive comment (SSE-09).
pub(crate) const KEEPALIVE: &[u8] = b": keep-alive\n\n";

/// The SSE encoder.
///
/// ```
/// use agui_sdk::lifecycle::RunStarted;
/// use agui_sdk::server::encode::sse::SseEncoder;
/// use agui_sdk::Event;
///
/// let mut encoder = SseEncoder::new();
/// let mut out = Vec::new();
/// encoder.encode(&Event::RunStarted(RunStarted::new("t-1", "r-1")), &mut out)?;
/// assert_eq!(
///     out,
///     b"data: {\"type\":\"RUN_STARTED\",\"threadId\":\"t-1\",\"runId\":\"r-1\"}\n\n"
/// );
/// # Ok::<(), agui_sdk::Error>(())
/// ```
#[derive(Debug, Default)]
pub struct SseEncoder {
    _private: (),
}

impl SseEncoder {
    /// An encoder.
    #[must_use]
    pub fn new() -> Self {
        SseEncoder { _private: () }
    }

    /// Appends one SSE event. The JSON is written without newlines, so it is
    /// always a single `data:` line.
    ///
    /// # Errors
    ///
    /// [`ErrorCode::Encode`].
    pub fn encode(&mut self, event: &Event, out: &mut Vec<u8>) -> Result<()> {
        let start = out.len();
        out.extend_from_slice(DATA);
        let body = out.len();
        if let Err(error) = serde_json::to_writer(&mut *out, event) {
            out.truncate(start);
            return Err(Error::new(
                ErrorCode::Encode,
                format!(
                    "{} could not be serialized for SSE",
                    event.event_type().as_str()
                ),
            )
            .with_source(error));
        }
        if has_line_break(&out[body..]) {
            split_lines(out, body);
        }
        out.extend_from_slice(b"\n\n");
        Ok(())
    }

    /// Appends a keep-alive comment.
    pub fn keepalive(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(KEEPALIVE);
    }
}

/// Whether `bytes` holds a CR or LF. Eight bytes a step (SWAR on a `u64`,
/// no `unsafe`): this check runs over every encoded byte, and compact JSON
/// almost never has a line break, so it is a full scan each time.
fn has_line_break(bytes: &[u8]) -> bool {
    const ONES: u64 = 0x0101_0101_0101_0101;
    const HIGH: u64 = 0x8080_8080_8080_8080;
    let mut words = bytes.chunks_exact(8);
    for word in &mut words {
        let mut w = [0; 8];
        w.copy_from_slice(word);
        let x = u64::from_le_bytes(w);
        let lf = x ^ (ONES * u64::from(b'\n'));
        let cr = x ^ (ONES * u64::from(b'\r'));
        if ((lf.wrapping_sub(ONES) & !lf) | (cr.wrapping_sub(ONES) & !cr)) & HIGH != 0 {
            return true;
        }
    }
    words.remainder().iter().any(|b| *b == b'\n' || *b == b'\r')
}

/// Rewrites the payload at `out[body..]` so that every line (split at CRLF,
/// CR or LF) is its own `data:` line, terminated by LF only. The caller has
/// already written the first `data: ` prefix. Cold path.
#[cold]
fn split_lines(out: &mut Vec<u8>, body: usize) {
    let payload = out.split_off(body);
    let mut bytes = payload.iter().copied().peekable();
    while let Some(byte) = bytes.next() {
        match byte {
            b'\r' | b'\n' => {
                if byte == b'\r' && bytes.peek() == Some(&b'\n') {
                    bytes.next();
                }
                out.push(b'\n');
                out.extend_from_slice(DATA);
            }
            other => out.push(other),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn line_breaks_are_found_at_every_offset() {
        for len in 0..40 {
            let mut bytes = vec![b'a'; len];
            assert!(!has_line_break(&bytes));
            for at in 0..len {
                for brk in *b"\n\r" {
                    bytes[at] = brk;
                    assert!(has_line_break(&bytes), "{brk} at {at} of {len}");
                    bytes[at] = 0x8A; // high bit set, low bits of LF
                    assert!(!has_line_break(&bytes));
                    bytes[at] = b'a';
                }
            }
        }
    }

    #[test]
    fn embedded_line_breaks_become_separate_data_lines() {
        let mut out = b"data: ".to_vec();
        out.extend_from_slice(b"a\nb\r\nc\rd");
        split_lines(&mut out, DATA.len());
        assert_eq!(out, b"data: a\ndata: b\ndata: c\ndata: d");
    }
}
