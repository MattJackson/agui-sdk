//! Protobuf wire primitives.

use super::codec_error::{Error, Result};

/// Wire type: varint.
pub const VARINT: u8 = 0;
/// Wire type: 64-bit.
pub const I64: u8 = 1;
/// Wire type: length-delimited.
pub const LEN: u8 = 2;
/// Wire type: start group (deprecated, skipped).
pub const SGROUP: u8 = 3;
/// Wire type: end group.
pub const EGROUP: u8 = 4;
/// Wire type: 32-bit.
pub const I32: u8 = 5;

/// Bytes a varint encoding of `v` takes.
#[inline]
pub fn varint_len(v: u64) -> usize {
    // 1 + floor(bits / 7), computed branch-free.
    ((64 - (v | 1).leading_zeros() as usize) * 9 + 64) / 64
}

/// Appends a varint.
#[inline]
pub fn put_varint(out: &mut Vec<u8>, mut v: u64) {
    if v < 0x80 {
        out.push(v as u8);
        return;
    }
    let mut buf = [0u8; 10];
    let mut i = 0;
    while v >= 0x80 {
        buf[i] = (v as u8) | 0x80;
        v >>= 7;
        i += 1;
    }
    buf[i] = v as u8;
    out.extend_from_slice(&buf[..=i]);
}

/// A field as it appears on the wire.
#[derive(Clone, Copy, Debug, Default)]
pub struct WireField {
    /// Field number.
    pub number: u32,
    /// Wire type.
    pub wt: u8,
    /// Varint / fixed value (for `VARINT`, `I64`, `I32`).
    pub v: u64,
    /// Payload range within the message (for `LEN`).
    pub start: usize,
    /// End of the payload range.
    pub end: usize,
}

/// Iterates the fields of one message, validating framing as it goes.
#[derive(Clone, Debug)]
pub struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    /// A reader over `buf`.
    #[cfg(test)]
    #[inline]
    pub fn new(buf: &'a [u8]) -> Self {
        Reader { buf, pos: 0 }
    }

    /// A reader over `buf[start..end]` whose field positions are offsets in
    /// `buf`, so nested messages are read in place without re-basing.
    #[inline]
    pub fn span(buf: &'a [u8], start: usize, end: usize) -> Self {
        let end = end.min(buf.len());
        Reader {
            buf: &buf[..end],
            pos: start.min(end),
        }
    }

    #[inline]
    fn varint(&mut self) -> Result<u64> {
        let b = self.buf;
        match b.get(self.pos) {
            None => return Err(Error::Truncated),
            Some(&c) if c < 0x80 => {
                self.pos += 1;
                return Ok(c as u64);
            }
            _ => {}
        }
        let mut v: u64 = 0;
        for i in 0..10 {
            let c = *b.get(self.pos + i).ok_or(Error::Truncated)?;
            if i == 9 && c > 1 {
                return Err(Error::InvalidVarint);
            }
            v |= ((c & 0x7f) as u64) << (7 * i);
            if c < 0x80 {
                self.pos += i + 1;
                return Ok(v);
            }
        }
        Err(Error::InvalidVarint)
    }

    #[inline]
    fn take(&mut self, n: usize) -> Result<usize> {
        let start = self.pos;
        if n > self.buf.len() - start {
            return Err(Error::Truncated);
        }
        self.pos += n;
        Ok(start)
    }

    fn tag(&mut self) -> Result<(u32, u8)> {
        let t = self.varint()?;
        let wt = (t & 7) as u8;
        let n = t >> 3;
        if n == 0 {
            return Err(Error::InvalidFieldNumber);
        }
        if n > u32::MAX as u64 {
            return Err(Error::InvalidFieldNumber);
        }
        Ok((n as u32, wt))
    }

    fn skip_group(&mut self, number: u32, depth: usize) -> Result<()> {
        if depth > super::decoder::DECODE_MAX_DEPTH {
            return Err(Error::DepthLimit);
        }
        loop {
            if self.pos >= self.buf.len() {
                return Err(Error::Truncated);
            }
            let (n, wt) = self.tag()?;
            if wt == EGROUP {
                return if n == number {
                    Ok(())
                } else {
                    Err(Error::InvalidWireType(EGROUP))
                };
            }
            self.value(n, wt, depth + 1)?;
        }
    }

    fn value(&mut self, number: u32, wt: u8, depth: usize) -> Result<WireField> {
        let mut f = WireField {
            number,
            wt,
            v: 0,
            start: 0,
            end: 0,
        };
        match wt {
            VARINT => f.v = self.varint()?,
            I64 => {
                let s = self.take(8)?;
                let mut a = [0u8; 8];
                a.copy_from_slice(&self.buf[s..s + 8]);
                f.v = u64::from_le_bytes(a);
            }
            I32 => {
                let s = self.take(4)?;
                let mut a = [0u8; 4];
                a.copy_from_slice(&self.buf[s..s + 4]);
                f.v = u32::from_le_bytes(a) as u64;
            }
            LEN => {
                let n = self.varint()?;
                let n = usize::try_from(n).map_err(|_| Error::Truncated)?;
                f.start = self.take(n)?;
                f.end = f.start + n;
            }
            SGROUP => self.skip_group(number, depth)?,
            _ => return Err(Error::InvalidWireType(wt)),
        }
        Ok(f)
    }

    /// The next field, or `None` at the end of the message.
    #[inline]
    pub fn next_field(&mut self) -> Result<Option<WireField>> {
        if self.pos >= self.buf.len() {
            return Ok(None);
        }
        let (n, wt) = self.tag()?;
        self.value(n, wt, 0).map(Some)
    }
}

/// Validates that `buf` is a well-formed message (framing only).
#[cfg(test)]
fn validate(buf: &[u8]) -> Result<()> {
    let mut r = Reader::new(buf);
    while r.next_field()?.is_some() {}
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    #[test]
    fn varint_lengths() {
        for (v, n) in [
            (0u64, 1),
            (127, 1),
            (128, 2),
            (16383, 2),
            (16384, 3),
            (u64::MAX, 10),
        ] {
            assert_eq!(varint_len(v), n, "{v}");
            let mut o = Vec::new();
            put_varint(&mut o, v);
            assert_eq!(o.len(), n);
            let mut r = Reader::new(&o);
            assert_eq!(r.varint().unwrap(), v);
        }
        for v in [
            1u64 << 14,
            1 << 21,
            1 << 28,
            1 << 35,
            1 << 42,
            1 << 49,
            1 << 56,
            1 << 63,
        ] {
            assert_eq!(varint_len(v), varint_len(v - 1) + 1);
        }
    }

    #[test]
    fn bad_varints() {
        let mut r = Reader::new(&[0xff; 11]);
        assert_eq!(r.varint(), Err(Error::InvalidVarint));
        let mut r = Reader::new(&[0x80]);
        assert_eq!(r.varint(), Err(Error::Truncated));
    }

    #[test]
    fn groups_are_skipped() {
        // field 5 start group, field 1 varint 1, field 5 end group, field 2 varint 3
        let b = [0x2b, 0x08, 0x01, 0x2c, 0x10, 0x03];
        let mut r = Reader::new(&b);
        let f = r.next_field().unwrap().unwrap();
        assert_eq!(f.number, 5);
        let f = r.next_field().unwrap().unwrap();
        assert_eq!((f.number, f.v), (2, 3));
        assert!(validate(&[0x2b, 0x08, 0x01]).is_err());
        assert!(validate(&[0x2b, 0x34]).is_err());
    }
}
