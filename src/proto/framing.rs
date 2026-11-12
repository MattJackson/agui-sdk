//! Length-prefixed framing: a 4-byte unsigned big-endian length, then exactly
//! that many bytes of one encoded event, no separator (PB-08).

use std::collections::VecDeque;

use bytes::{Buf, Bytes, BytesMut};

use crate::error::{Error, ErrorCode, Result};
use crate::wire::Chunk;

/// Default largest accepted frame payload: 16 MiB.
pub const DEFAULT_MAX_FRAME_BYTES: usize = crate::pipeline::DEFAULT_MAX_FRAME_BYTES;

/// Bytes in a frame's length prefix.
const HEADER: usize = 4;

/// Appends one frame (prefix plus payload) to `out`.
///
/// # Errors
///
/// [`ErrorCode::FrameTooLarge`] for a payload over `u32::MAX` bytes.
pub fn write_frame(payload: &[u8], out: &mut Vec<u8>) -> Result<()> {
    let len = u32::try_from(payload.len()).map_err(|_| {
        Error::new(
            ErrorCode::FrameTooLarge,
            format!(
                "a {}-byte payload does not fit a 32-bit frame length",
                payload.len()
            ),
        )
    })?;
    out.reserve(HEADER + payload.len());
    out.extend_from_slice(&len.to_be_bytes());
    out.extend_from_slice(payload);
    Ok(())
}

/// Assembly capacity, in bytes, shared with an assembled frame for reuse.
const ASSEMBLY_KEEP_BYTES: usize = 64 * 1024;

/// Incremental frame decoder.
///
/// Feed transport chunks with [`push`](Self::push) and pull frames with
/// [`next_frame`](Self::next_frame). A frame that lies inside one chunk is
/// returned as a slice of that chunk (no copy); only a frame split across
/// chunks is assembled, into a buffer whose capacity is reused once the
/// previous assembled frame has been dropped; a buffer that grew past 64 KiB
/// goes with its frame instead, so one large frame does not keep its memory
/// held after it is dropped.
///
/// The length prefix is checked against the limit as soon as its four bytes
/// are buffered, before any payload is assembled; an oversized frame is an
/// error, not a wait. Calling `next_frame` after every `push` therefore keeps
/// the bytes held for a refused frame to at most one chunk.
///
/// At end of body, [`has_partial`](Self::has_partial) tells a truncated run
/// (bytes of an incomplete frame are buffered, PB-10) from a clean end.
///
/// No `Default`: a zero limit would reject every non-empty frame; use
/// [`FrameDecoder::new`] with [`DEFAULT_MAX_FRAME_BYTES`] or your budget.
#[derive(Debug)]
pub struct FrameDecoder {
    max_frame_bytes: usize,
    /// Received bytes not yet returned, oldest first; never holds an empty chunk.
    chunks: VecDeque<Bytes>,
    /// Total length of `chunks` (not of `assembly`).
    buffered: usize,
    /// Assembly buffer for frames that span chunks.
    assembly: BytesMut,
    /// The announced payload length of the frame being assembled, once its
    /// header has been consumed: later chunks are copied straight into
    /// `assembly` instead of queued, so a peer sending one byte per chunk
    /// costs the frame's bytes, not a handle per byte.
    pending: Option<usize>,
}

impl FrameDecoder {
    /// A decoder accepting frames up to `max_frame_bytes`.
    #[must_use]
    pub fn new(max_frame_bytes: usize) -> Self {
        FrameDecoder {
            max_frame_bytes,
            chunks: VecDeque::new(),
            buffered: 0,
            assembly: BytesMut::new(),
            pending: None,
        }
    }

    /// Adds received bytes.
    pub fn push(&mut self, chunk: Chunk) {
        let mut chunk = chunk.into_bytes();
        if let Some(len) = self.pending {
            let take = (len - self.assembly.len()).min(chunk.len());
            self.assembly.extend_from_slice(&chunk[..take]);
            chunk.advance(take);
        }
        if !chunk.is_empty() {
            self.buffered += chunk.len();
            self.chunks.push_back(chunk);
        }
    }

    /// The next complete frame's payload, if one is buffered.
    ///
    /// # Errors
    ///
    /// [`ErrorCode::FrameTooLarge`] when a
    /// length prefix exceeds the limit (checked before buffering the payload).
    pub fn next_frame(&mut self) -> Result<Option<Chunk>> {
        if let Some(len) = self.pending {
            if self.assembly.len() < len {
                return Ok(None);
            }
            self.pending = None;
            return Ok(Some(self.assembled()));
        }
        let Some(len) = self.header()? else {
            return Ok(None);
        };
        if self.buffered - HEADER < len {
            // Incomplete: move what is queued into the assembly buffer and
            // take the rest of the frame there as it arrives.
            self.skip(HEADER);
            self.buffered -= HEADER;
            // No reservation from the announced length: memory tracks the
            // bytes actually received, not the peer's claim.
            while let Some(front) = self.chunks.pop_front() {
                self.assembly.extend_from_slice(&front);
            }
            self.buffered = 0;
            self.pending = Some(len);
            return Ok(None);
        }
        self.buffered -= HEADER + len;
        if let Some(front) = self.chunks.front_mut() {
            if front.len() >= HEADER + len {
                // Zero-copy: the whole frame lies in one chunk.
                front.advance(HEADER);
                let frame = front.split_to(len);
                if front.is_empty() {
                    self.chunks.pop_front();
                }
                return Ok(Some(Chunk::from_bytes(frame)));
            }
        }
        self.skip(HEADER);
        self.assembly.reserve(len);
        let mut need = len;
        while need > 0 {
            let Some(front) = self.chunks.front_mut() else {
                break;
            };
            let take = need.min(front.len());
            self.assembly.extend_from_slice(&front[..take]);
            front.advance(take);
            need -= take;
            if front.is_empty() {
                self.chunks.pop_front();
            }
        }
        Ok(Some(self.assembled()))
    }

    /// The assembled frame. A buffer within [`ASSEMBLY_KEEP_BYTES`] is
    /// shared with the frame and reclaimed once the frame is dropped; a larger
    /// one is handed to the frame outright, so it is freed with the frame and
    /// not held for the life of the stream.
    fn assembled(&mut self) -> Chunk {
        let frame = if self.assembly.capacity() > ASSEMBLY_KEEP_BYTES {
            std::mem::take(&mut self.assembly)
        } else {
            self.assembly.split()
        };
        Chunk::from_bytes(frame.freeze())
    }

    /// Whether bytes of an incomplete frame are buffered. At end of body this
    /// means the run was truncated, not that decoding failed (PB-10).
    #[must_use]
    pub fn has_partial(&self) -> bool {
        self.buffered > 0 || self.pending.is_some()
    }

    /// The announced payload length of the buffered frame, checked against
    /// the limit; `None` while fewer than four bytes are buffered.
    fn header(&self) -> Result<Option<usize>> {
        if self.buffered < HEADER {
            return Ok(None);
        }
        let mut prefix = [0u8; HEADER];
        let mut filled = 0;
        for chunk in &self.chunks {
            let take = (HEADER - filled).min(chunk.len());
            prefix[filled..filled + take].copy_from_slice(&chunk[..take]);
            filled += take;
            if filled == HEADER {
                break;
            }
        }
        let announced = u32::from_be_bytes(prefix);
        match usize::try_from(announced) {
            Ok(len) if len <= self.max_frame_bytes && len <= usize::MAX - HEADER => Ok(Some(len)),
            _ => Err(Error::new(
                ErrorCode::FrameTooLarge,
                format!(
                    "frame of {announced} bytes exceeds the {}-byte limit",
                    self.max_frame_bytes
                ),
            )),
        }
    }

    /// Drops `n` buffered bytes (already subtracted from `buffered`).
    fn skip(&mut self, mut n: usize) {
        while n > 0 {
            let Some(front) = self.chunks.front_mut() else {
                return;
            };
            let take = n.min(front.len());
            front.advance(take);
            n -= take;
            if front.is_empty() {
                self.chunks.pop_front();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    fn stream(payloads: &[&[u8]]) -> Vec<u8> {
        let mut out = Vec::new();
        for p in payloads {
            write_frame(p, &mut out).unwrap();
        }
        out
    }

    #[test]
    fn frames_in_one_chunk_are_slices_of_it() {
        let bytes = Chunk::from(stream(&[b"abc", b"defg"]));
        let base = bytes.as_ptr() as usize;
        let mut d = FrameDecoder::new(DEFAULT_MAX_FRAME_BYTES);
        d.push(bytes);
        let a = d.next_frame().unwrap().unwrap();
        let b = d.next_frame().unwrap().unwrap();
        assert_eq!((&a[..], &b[..]), (&b"abc"[..], &b"defg"[..]));
        assert_eq!(a.as_ptr() as usize, base + 4);
        assert_eq!(b.as_ptr() as usize, base + 11);
        assert!(d.next_frame().unwrap().is_none());
        assert!(!d.has_partial());
    }

    #[test]
    fn announced_length_is_not_reserved_up_front() {
        let mut d = FrameDecoder::new(DEFAULT_MAX_FRAME_BYTES);
        let mut bytes = (8u32 * 1024 * 1024).to_be_bytes().to_vec();
        bytes.push(1);
        d.push(Chunk::from(bytes));
        assert!(d.next_frame().unwrap().is_none());
        assert!(d.has_partial());
        assert!(
            d.assembly.capacity() < 64 * 1024,
            "held {} bytes for a 1-byte payload",
            d.assembly.capacity()
        );
    }

    #[cfg(target_pointer_width = "32")]
    #[test]
    fn announced_length_that_would_overflow_the_header_sum_is_too_large() {
        let mut d = FrameDecoder::new(usize::MAX);
        d.push(Chunk::from(u32::MAX.to_be_bytes().to_vec()));
        assert_eq!(d.next_frame().unwrap_err().code(), ErrorCode::FrameTooLarge);
    }

    #[test]
    fn assembly_buffer_is_reused_once_the_frame_is_dropped() {
        let bytes = stream(&[&[7; 64], &[8; 64]]);
        let mut d = FrameDecoder::new(DEFAULT_MAX_FRAME_BYTES);
        let mut first = None;
        for (i, piece) in bytes.chunks(10).enumerate() {
            d.push(Chunk::copy_from_slice(piece));
            if let Some(f) = d.next_frame().unwrap() {
                let at = f.as_ptr() as usize;
                assert!(f.iter().all(|&b| b == f[0]), "chunk {i}");
                match first {
                    None => first = Some(at),
                    Some(p) => assert_eq!(p, at, "second frame reuses the buffer"),
                }
            }
        }
        assert!(first.is_some());
    }

    /// A large assembled frame owns its buffer outright: the decoder keeps no
    /// reference to it, so once the frame is dropped the memory is freed even
    /// if every later frame arrives whole and never touches the assembly.
    #[test]
    fn a_large_assembled_frame_does_not_pin_its_buffer() {
        let payload = vec![5u8; 1024 * 1024];
        let bytes = stream(&[&payload, b"x"]);
        let mut d = FrameDecoder::new(DEFAULT_MAX_FRAME_BYTES);
        let mut frame = None;
        for piece in bytes[..bytes.len() - 5].chunks(4096) {
            d.push(Chunk::copy_from_slice(piece));
            if let Some(f) = d.next_frame().unwrap() {
                frame = Some(f);
            }
        }
        let frame = frame.unwrap().into_bytes();
        assert_eq!(frame.len(), payload.len());
        assert!(
            frame.is_unique(),
            "the decoder still references the frame's buffer"
        );
        assert_eq!(d.assembly.capacity(), 0);
        d.push(Chunk::copy_from_slice(&bytes[bytes.len() - 5..]));
        assert_eq!(&d.next_frame().unwrap().unwrap()[..], b"x");
    }

    #[test]
    fn default_limit_is_sixteen_mebibytes() {
        assert_eq!(
            FrameDecoder::new(DEFAULT_MAX_FRAME_BYTES).max_frame_bytes,
            DEFAULT_MAX_FRAME_BYTES
        );
    }

    /// proto-3: the bytes of a frame still arriving are copied into the
    /// assembly buffer as they come, not queued as one handle per chunk.
    #[test]
    fn a_frame_trickled_in_tiny_chunks_holds_no_handle_per_chunk() {
        let payload: Vec<u8> = (0..65_536u32).map(|i| (i % 251) as u8).collect();
        let bytes = stream(&[&payload]);
        let mut d = FrameDecoder::new(DEFAULT_MAX_FRAME_BYTES);
        let last = bytes.len() - 1;
        for (i, b) in bytes.iter().enumerate() {
            d.push(Chunk::copy_from_slice(&[*b]));
            let frame = d.next_frame().unwrap();
            if i < last {
                assert!(frame.is_none(), "byte {i}");
                assert!(
                    d.chunks.len() <= 3,
                    "{} handles after byte {i}",
                    d.chunks.len()
                );
            } else {
                assert_eq!(&frame.unwrap()[..], &payload[..]);
            }
        }
        assert!(!d.has_partial());
        assert!(d.next_frame().unwrap().is_none());
    }
}
