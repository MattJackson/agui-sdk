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
/// [`ErrorCode::FrameTooLarge`] for a payload of `u32::MAX` bytes or more.
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
/// about 16 times the payload's size (31 bytes of
/// `{"interruptId":"","status":""},` per 2-byte empty resume entry), which
/// `Processor::push_json` then budgets as it parses.
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

    /// The `Event` envelope of `json`, with `extra` appended to the arm's
    /// body (its length prefix rewritten).
    fn envelope_with(json: &str, extra: &[u8]) -> Vec<u8> {
        use super::super::wire::{put_varint, Reader};
        let mut bytes = Vec::new();
        crate::proto::encode::encode_json(json.as_bytes(), &mut bytes).unwrap();
        let f = Reader::new(&bytes).next_field().unwrap().unwrap();
        let mut body = bytes[f.start..f.end].to_vec();
        body.extend_from_slice(extra);
        let mut out = Vec::new();
        put_varint(&mut out, u64::from(f.number) << 3 | 2);
        put_varint(&mut out, body.len() as u64);
        out.extend_from_slice(&body);
        out
    }

    const RUN_STARTED: &str = r#"{"type":"RUN_STARTED","threadId":"t","runId":"r"}"#;

    /// A field number above protobuf's maximum (2^29 - 1) is malformed
    /// bytes (PB-18), not an unknown field to skip (PB-15).
    #[test]
    fn a_field_number_above_the_protobuf_maximum_is_malformed() {
        use super::super::wire::put_varint;
        let tagged = |n: u64| {
            let mut extra = Vec::new();
            put_varint(&mut extra, n << 3);
            extra.push(1);
            envelope_with(RUN_STARTED, &extra)
        };
        assert!(matches!(
            decode_event(&tagged((1 << 29) - 1)),
            Ok(Decoded::Event(_))
        ));
        for n in [1u64 << 29, 1 << 30, u64::from(u32::MAX), 1 << 40] {
            let e = decode_event(&tagged(n)).unwrap_err();
            assert_eq!(e.code(), ErrorCode::ProtoFraming, "field {n}");
            assert!(!e.to_string().contains("field number 0"), "{e}");
        }
    }

    /// A length-delimited field `n` holding `body`.
    fn len_field(n: u64, body: &[u8]) -> Vec<u8> {
        use super::super::wire::put_varint;
        let mut out = Vec::new();
        put_varint(&mut out, n << 3 | 2);
        put_varint(&mut out, body.len() as u64);
        out.extend_from_slice(body);
        out
    }

    /// Records in `fails` unless `bytes` decode as malformed.
    fn expect_malformed(fails: &mut Vec<String>, bytes: &[u8], what: &str) {
        match decode_event(bytes) {
            Err(e) if e.code() == ErrorCode::ProtoFraming => {}
            Err(e) => fails.push(format!("{what}: {e}")),
            Ok(_) => fails.push(format!("{what}: decoded")),
        }
    }

    /// Bytes a protobuf parser would reject are malformed even when a later
    /// occurrence, arm or duplicate key supersedes them (PB-18): every
    /// occurrence is parsed, as protobuf parses it, not just the one kept.
    #[test]
    fn superseded_occurrences_are_still_parsed() {
        let fails = &mut Vec::new();
        // A singular string: `run_id` as invalid UTF-8, then "r".
        let mut extra = len_field(3, &[0xff]);
        extra.extend(len_field(3, b"r"));
        expect_malformed(
            fails,
            &envelope_with(RUN_STARTED, &extra),
            "superseded string",
        );
        assert!(decode_event(&envelope_with(RUN_STARTED, &len_field(3, b"r"))).is_ok());

        // An envelope arm superseded by another: TEXT_MESSAGE_START whose
        // `message_id` is invalid UTF-8, then a valid STATE_SNAPSHOT; and one
        // whose bytes do not frame.
        let snapshot = envelope_with(r#"{"type":"STATE_SNAPSHOT","snapshot":{}}"#, &[]);
        for (bad, what) in [
            (
                len_field(1, &len_field(2, &[0xff])),
                "superseded arm string",
            ),
            (len_field(1, &[0x0a, 0xff]), "superseded arm framing"),
        ] {
            let mut bytes = bad;
            bytes.extend_from_slice(&snapshot);
            expect_malformed(fails, &bytes, what);
        }

        // `snapshot` Values. string_value 0xff, then null_value.
        let snap = |value: Vec<u8>| {
            let mut env = Vec::new();
            let body = len_field(2, &value);
            env.extend(len_field(7, &body));
            env
        };
        let mut v = len_field(3, &[0xff]);
        v.extend([0x08, 0x00]);
        expect_malformed(fails, &snap(v), "superseded Value kind");
        // string_value 0xff, then string_value "s".
        let mut v = len_field(3, &[0xff]);
        v.extend(len_field(3, b"s"));
        expect_malformed(fails, &snap(v), "superseded string_value");

        // Struct entries: key "a" with string_value 0xff, then key "a"
        // again with null; with few entries and with many (sorted path).
        let entry = |key: &[u8], value: &[u8]| {
            let mut e = len_field(1, key);
            e.extend(len_field(2, value));
            len_field(1, &e)
        };
        for pad in [0usize, 40] {
            let mut st = entry(b"a", &len_field(3, &[0xff]));
            for i in 0..pad {
                st.extend(entry(format!("k{i}").as_bytes(), &[0x08, 0x00]));
            }
            st.extend(entry(b"a", &[0x08, 0x00]));
            expect_malformed(fails, &snap(len_field(5, &st)), "superseded duplicate key");
            let mut ok = entry(b"a", &len_field(3, b"s"));
            ok.extend(entry(b"a", &[0x08, 0x00]));
            assert!(decode_event(&snap(len_field(5, &ok))).is_ok());
        }
        // An entry whose key is repeated: 0xff, then "a".
        let mut e = len_field(1, &[0xff]);
        e.extend(len_field(1, b"a"));
        expect_malformed(
            fails,
            &snap(len_field(5, &len_field(1, &e))),
            "superseded key",
        );

        // A known field the JSON form does not show: `RUN_FINISHED`'s
        // `interrupts` with no outcome, holding an invalid string.
        let finished = r#"{"type":"RUN_FINISHED","threadId":"t","runId":"r"}"#;
        let interrupt = len_field(1, &[0xff]);
        expect_malformed(
            fails,
            &envelope_with(finished, &len_field(6, &interrupt)),
            "unshown outcome member",
        );
        // A `content` field left out because another content field wins.
        let result = r#"{"type":"TOOL_CALL_RESULT","messageId":"m","toolCallId":"c","content":[{"type":"text","text":"x"}]}"#;
        assert!(decode_event(&envelope_with(result, &[])).is_ok());
        expect_malformed(
            fails,
            &envelope_with(result, &len_field(5, &[0xff])),
            "unshown content field",
        );
        assert!(fails.is_empty(), "accepted: {fails:#?}");
    }

    /// The expansion `decode_event_json` documents: its worst shape, a run
    /// input of empty resume entries, is about 16 times the payload.
    #[test]
    fn json_text_is_at_most_about_16_times_the_payload() {
        let n = 10_000;
        let input = len_field(5, &[0x4a, 0x00].repeat(n));
        let bytes = len_field(12, &input);
        let mut json = String::new();
        assert_eq!(
            decode_event_json(&bytes, &mut json).unwrap(),
            DecodedJson::Event("RUN_STARTED")
        );
        assert!(json.contains(r#"{"interruptId":"","status":""},"#));
        let ratio = json.len() as f64 / bytes.len() as f64;
        assert!((15.0..=16.0).contains(&ratio), "{ratio}");
    }

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
