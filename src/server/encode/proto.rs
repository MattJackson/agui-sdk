//! Protobuf encoding: each event as one length-prefixed frame (PB-08) via
//! [`crate::proto`].
//!
//! The frame is written in place: four bytes are reserved in the caller's
//! buffer, the event is encoded after them by
//! [`proto::encode::encode_event`](crate::proto::encode::encode_event) (which
//! reuses per-thread scratch), and the big-endian length is filled in. No
//! intermediate payload buffer, no allocation once `out` has grown.
//!
//! The payload is byte-identical to
//! [`encode_json`](crate::proto::encode::encode_json) of the event's JSON text,
//! which reproduces the canonical byte corpus; from typed events the
//! open-object entry order matches the corpus only with the crate feature
//! `preserve-order` (PB-12, decision D4).

use crate::error::{Error, ErrorCode, Result};
use crate::event::Event;

/// Bytes in a frame's length prefix.
const HEADER: usize = 4;

/// The protobuf encoder.
///
/// ```
/// use agui_sdk::lifecycle::RunStarted;
/// use agui_sdk::server::encode::proto::ProtoEncoder;
/// use agui_sdk::Event;
///
/// let mut encoder = ProtoEncoder::new();
/// let mut out = Vec::new();
/// encoder.encode(&Event::RunStarted(RunStarted::new("t-1", "r-1")), &mut out)?;
/// // A 4-byte big-endian length, then the message.
/// let len = u32::from_be_bytes([out[0], out[1], out[2], out[3]]) as usize;
/// assert_eq!(out.len(), 4 + len);
/// # Ok::<(), agui_sdk::Error>(())
/// ```
#[derive(Debug, Default)]
pub struct ProtoEncoder {
    _private: (),
}

impl ProtoEncoder {
    /// An encoder.
    #[must_use]
    pub fn new() -> Self {
        ProtoEncoder { _private: () }
    }

    /// Appends one frame.
    ///
    /// # Errors
    ///
    /// [`ErrorCode::Encode`].
    pub fn encode(&mut self, event: &Event, out: &mut Vec<u8>) -> Result<()> {
        let start = out.len();
        out.extend_from_slice(&[0; HEADER]);
        if let Err(error) = crate::proto::encode::encode_event(event, out) {
            out.truncate(start);
            return Err(error);
        }
        let payload = out.len() - start - HEADER;
        let Ok(length) = u32::try_from(payload) else {
            out.truncate(start);
            return Err(Error::new(
                ErrorCode::Encode,
                format!(
                    "{} encodes to {payload} bytes, more than a frame can carry",
                    event.event_type().as_str()
                ),
            ));
        };
        out[start..start + HEADER].copy_from_slice(&length.to_be_bytes());
        Ok(())
    }
}
