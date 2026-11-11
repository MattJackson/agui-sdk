//! Protobuf → JSON event decoding.
//!
//! The decoder scans each message once in place: strings are validated as
//! UTF-8 where they lie and copied once, into the output. Fields are emitted
//! in a fixed order whatever their order on the wire; for a field seen more
//! than once the last scalar wins and message occurrences merge, as protobuf
//! requires. Open JSON comes back with JS semantics: object entries in wire
//! order with duplicates collapsed; `Value.number_value` (an `f64`) keeps JS
//! semantics, integral values within ±(2⁵³−1) as integers and non-finite
//! values as `null`. `int64` fields are exact; one outside ±(2⁵³−1) is passed
//! on for enforcement to judge, as on the JSON wire (PROC-06).
//!
//! Recursion is bounded, so hostile nesting fails with an error rather than
//! exhausting the stack.

use serde_json::Value;

use super::decoder::{with_dec, TextOut, Transcoded};
use crate::error::{Error, ErrorCode, Result};

/// The result of decoding one frame.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum Decoded {
    /// A known event, in its JSON form, ready for the compatibility boundary.
    /// Absent required scalars and lists are materialised empty, the
    /// documented protobuf exception (PROC-09).
    Event(Value),
    /// The envelope arm is newer than this build: drop with a warning
    /// (PROC-08, PB-17). An envelope with no arm at all is reported the same
    /// way with `field_number` 0, which no real arm can have.
    UnknownArm {
        /// The arm's field number.
        field_number: u32,
    },
}

/// What [`decode_event_json`] wrote.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum DecodedJson {
    /// A known event's JSON text was appended; this is its `type`.
    Event(&'static str),
    /// As [`Decoded::UnknownArm`]; nothing was written.
    UnknownArm {
        /// The arm's field number (0 for an envelope with no arm).
        field_number: u32,
    },
}

/// Decodes one frame's payload. Borrows the input; strings are copied once,
/// into the resulting value.
///
/// Objects in the value keep wire order only with the crate feature
/// `preserve-order`; [`decode_event_json`] keeps it in every build.
///
/// The output is bounded by the input size times a constant, not by
/// [`Limits`](crate::pipeline::Limits), and the constant is large: an empty
/// repeated sub-message is 2 bytes on the wire and a whole object once
/// decoded (a `MESSAGES_SNAPSHOT` of empty messages becomes
/// `{"id":"","role":""}` per 2 bytes), so a value can hold up to about 350 times
/// its frame's size in memory (an 8 MB frame, about 2.8 GB). To feed an
/// untrusted peer into a pipeline, use [`decode_event_json`] with
/// `Processor::push_json`, which budgets while parsing.
///
/// # Errors
///
/// [`ErrorCode::ProtoFraming`] when the bytes
/// do not decode as a message (PB-18);
/// [`ErrorCode::LimitExceeded`] when open
/// JSON nests deeper than 128 levels (the event object counted as 1);
/// [`ErrorCode::FrameTooLarge`] for a payload over `u32::MAX` bytes.
pub fn decode_event(payload: &[u8]) -> Result<Decoded> {
    decode_event_within(payload, usize::MAX)
}

/// [`decode_event`] producing at most `max_entries` container entries
/// (object members plus array elements, counted as `Limits::max_container_entries`
/// counts them): the client's decode path, so a frame cannot materialise
/// more than enforcement would accept.
pub(crate) fn decode_event_within(payload: &[u8], max_entries: usize) -> Result<Decoded> {
    let (t, value) = with_dec(|d| d.event_value(payload, max_entries))?;
    Ok(match t {
        Transcoded::Event(_) => match value {
            Some(v) => Decoded::Event(v),
            None => {
                return Err(Error::new(
                    ErrorCode::Internal,
                    "protobuf decoder produced no value",
                ))
            }
        },
        Transcoded::UnknownEvent { field_number } => Decoded::UnknownArm { field_number },
        Transcoded::Empty => Decoded::UnknownArm { field_number: 0 },
    })
}

/// Decodes one frame's payload straight to JSON text appended to `out`,
/// without building a tree: the path for feeding a JSON pipeline (for example
/// `Processor::push_json`) from the binary wire. Member order is wire order.
/// A warm decoder makes no allocations beyond growth of `out`.
///
/// On error `out` is left as it was.
///
/// Like [`decode_event`], not bounded by `Limits`: the text appended is up to
/// about 10 times the payload's size (20 bytes of `{"id":"","role":""},` per
/// 2-byte empty message), which `Processor::push_json` then budgets as it
/// parses.
///
/// # Errors
///
/// As [`decode_event`].
pub fn decode_event_json(payload: &[u8], out: &mut String) -> Result<DecodedJson> {
    let at = out.len();
    let r = {
        let mut w = TextOut::new(out);
        with_dec(|d| d.event(payload, &mut w, usize::MAX))
    };
    match r {
        Ok(Transcoded::Event(ty)) => Ok(DecodedJson::Event(ty)),
        Ok(Transcoded::UnknownEvent { field_number }) => {
            Ok(DecodedJson::UnknownArm { field_number })
        }
        Ok(Transcoded::Empty) => Ok(DecodedJson::UnknownArm { field_number: 0 }),
        Err(e) => {
            out.truncate(at);
            Err(e.into())
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use crate::diagnostics::{CollectingSink, Reporter};
    use crate::pipeline::enforce::Enforcer;
    use crate::pipeline::Limits;
    use std::sync::Arc;

    /// proto-1: the decoder's entry budget is the count the tree enforcer
    /// applies, so it refuses exactly what enforcement would, at the same N.
    #[test]
    fn entry_budget_matches_enforcement() {
        let src = r#"{"type":"STATE_SNAPSHOT","snapshot":{"a":1,"b":[1,2,{"c":null}],"d":{}}}"#;
        let mut bytes = Vec::new();
        crate::proto::encode::encode_json(src.as_bytes(), &mut bytes).unwrap();
        let Decoded::Event(full) = decode_event(&bytes).unwrap() else {
            panic!("not an event")
        };
        let mut seen_ok = false;
        let mut seen_err = false;
        for max in 1..=16usize {
            let limits = Limits {
                max_container_entries: max,
                ..Limits::default()
            };
            let reporter = &mut Reporter::new(Arc::new(CollectingSink::new()), 16);
            let oracle = Enforcer::new(limits).enforce_event_value(full.clone(), reporter);
            let decoded = decode_event_within(&bytes, max);
            assert_eq!(decoded.is_ok(), oracle.is_ok(), "max {max}");
            if let (Err(d), Err(o)) = (&decoded, &oracle) {
                assert_eq!(d.code(), o.code(), "max {max}");
                assert_eq!(d.code(), ErrorCode::LimitExceeded);
            }
            seen_ok |= decoded.is_ok();
            seen_err |= decoded.is_err();
        }
        assert!(seen_ok && seen_err, "the sweep must straddle the boundary");
    }
}
