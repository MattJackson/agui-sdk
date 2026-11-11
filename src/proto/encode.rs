//! JSON → protobuf event encoding.
//!
//! All encoders append to a caller-owned buffer so a server writing many
//! events reuses one allocation. Each call runs two passes over the same
//! borrowed tree: the first measures every length-delimited submessage, the
//! second writes into a buffer reserved to the exact final size, so no length
//! prefix is ever back-patched. The parse arena and size table live in a
//! per-thread scratch that is reused: a warm encoder allocates nothing beyond
//! growth of `out`, for any event whose scratch fits the 256 KiB kept between
//! encodes. That is a few thousand JSON values, depending on shape (measured:
//! up to about 8,000 numbers in one array, 3,300 members of one flat object,
//! or 2,000 one-member objects in an array). A larger event's scratch is
//! released when its encode ends, so encoding it again regrows it.
//!
//! On error `out` is left as it was before the call.

use std::cell::RefCell;

use serde_json::Value;

use super::encoder::Encoder;
use crate::error::{Error, ErrorCode, Result};
use crate::event::Event;

/// Capacity (bytes) kept in the per-thread text and string buffers between
/// encodes.
const KEEP_BYTES: usize = 256 * 1024;

thread_local! {
    static ENCODER: RefCell<Encoder> = RefCell::new(Encoder::new());
    static TEXT: RefCell<Vec<u8>> = const { RefCell::new(Vec::new()) };
    /// The typed builder's string arena.
    static STRINGS: RefCell<String> = const { RefCell::new(String::new()) };
}

/// Runs `f` with this thread's encoder scratch, or a fresh one if the scratch
/// is already in use further up the stack.
fn with_encoder<T>(f: impl FnOnce(&mut Encoder) -> T) -> T {
    ENCODER.with(|e| match e.try_borrow_mut() {
        Ok(mut e) => {
            let r = f(&mut e);
            e.trim();
            r
        }
        Err(_) => f(&mut Encoder::new()),
    })
}

/// Calls `f` and restores `out` to its prior length if it fails.
fn atomic(
    out: &mut Vec<u8>,
    f: impl FnOnce(&mut Vec<u8>) -> super::codec_error::Result<()>,
) -> Result<()> {
    let at = out.len();
    f(out).map_err(|e| {
        out.truncate(at);
        Error::from(e)
    })
}

/// Encodes a typed event, appending the message bytes (no length prefix) to `out`.
///
/// The event's `Serialize` output is built straight into the tree the
/// encoder walks (strings copied unescaped into a reused per-thread arena,
/// numbers kept as the `f64` `JSON.parse` would read), so typed fields need
/// no second code path and no JSON text is written and re-parsed. The bytes
/// are identical to [`encode_json`] of `serde_json::to_vec(event)`; a value
/// the builder does not mirror takes that text path instead. Open
/// objects (`state`, `metadata`, `CUSTOM.value`, …) are `serde_json` maps:
/// their entries are in insertion order, and the output byte-identical to the
/// canonical corpus, only with the crate feature `preserve-order` (see the
/// [module docs](super#byte-parity-and-open-object-order)).
///
/// # Errors
///
/// [`ErrorCode::Encode`] if a value cannot be
/// represented on the protobuf wire (e.g. a number outside `f64`).
pub fn encode_event(event: &Event, out: &mut Vec<u8>) -> Result<()> {
    let typed = STRINGS.with(|t| {
        let mut text = t.try_borrow_mut().ok()?;
        let done = with_encoder(|e| {
            let at = out.len();
            let done = e.encode_typed(event, &mut text, out)?;
            Some(done.map_err(|err| {
                out.truncate(at);
                Error::from(err)
            }))
        });
        text.clear();
        text.shrink_to(KEEP_BYTES);
        done
    });
    match typed {
        Some(done) => done,
        None => encode_event_text(event, out),
    }
}

/// [`encode_event`] through JSON text: the reference the typed builder
/// matches, and its fallback.
fn encode_event_text(event: &Event, out: &mut Vec<u8>) -> Result<()> {
    let mut encode = |text: &mut Vec<u8>| -> Result<()> {
        text.clear();
        serde_json::to_writer(&mut *text, event)
            .map_err(|e| Error::new(ErrorCode::Encode, format!("event serialization: {e}")))?;
        encode_json(text, out)
    };
    TEXT.with(|t| match t.try_borrow_mut() {
        Ok(mut text) => {
            let r = encode(&mut text);
            text.clear();
            text.shrink_to(KEEP_BYTES);
            r
        }
        Err(_) => encode(&mut Vec::new()),
    })
}

/// Encodes an event given as a JSON value, appending to `out`.
///
/// Open-object entries follow the map's iteration order, which is insertion
/// order only with the crate feature `preserve-order` (see the
/// [module docs](super#byte-parity-and-open-object-order)).
///
/// # Errors
///
/// As [`encode_event`], plus [`ErrorCode::MalformedEvent`]
/// when the value is not an event object of a known type.
pub fn encode_value(event: &Value, out: &mut Vec<u8>) -> Result<()> {
    with_encoder(|e| atomic(out, |out| e.encode_value_into(event, out)))
}

/// Encodes an event given as JSON text, preserving the text's member order
/// for open objects. This is the byte-parity path (PB-12, ARCH-06). Parses
/// once, streaming, without building a `Value`.
///
/// The text is parsed as `JSON.parse` would: strictly, numbers rounded to the
/// nearest `f64`, a repeated key keeping its first position and last value.
///
/// # Errors
///
/// As [`encode_value`], plus [`ErrorCode::Json`] for invalid JSON.
pub fn encode_json(json: &[u8], out: &mut Vec<u8>) -> Result<()> {
    let text = std::str::from_utf8(json)
        .map_err(|e| Error::new(ErrorCode::Json, format!("invalid UTF-8 in JSON text: {e}")))?;
    with_encoder(|e| atomic(out, |out| e.encode_into(text, out)))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use serde_json::json;

    /// proto-17: one huge event must not leave its scratch capacity on the
    /// thread for good.
    #[test]
    fn encoder_scratch_does_not_keep_the_capacity_of_a_huge_event() {
        std::thread::spawn(|| {
            let members: Vec<String> = (0..50_000)
                .map(|i| format!(r#""key-{i:08}":{i}"#))
                .collect();
            let json = format!(
                r#"{{"type":"STATE_SNAPSHOT","snapshot":{{{}}}}}"#,
                members.join(",")
            );
            let mut out = Vec::new();
            encode_json(json.as_bytes(), &mut out).unwrap();
            let event: Event = serde_json::from_str(&json).unwrap();
            encode_event(&event, &mut out).unwrap();
            encode_event_text(&event, &mut out).unwrap();
            ENCODER.with(|e| {
                let cap = e.borrow().max_capacity();
                assert!(
                    cap <= super::super::encoder::SCRATCH_KEEP_ENTRIES,
                    "scratch {cap}"
                );
                let bytes = e.borrow().retained_bytes();
                assert!(
                    bytes <= super::super::encoder::SCRATCH_KEEP_BYTES,
                    "scratch {bytes} bytes"
                );
            });
            STRINGS.with(|s| assert!(s.borrow().capacity() <= KEEP_BYTES));
            TEXT.with(|t| assert!(t.borrow().capacity() <= KEEP_BYTES));
        })
        .join()
        .unwrap();
    }

    /// An event over `SCRATCH_KEEP_ENTRIES` values but within the byte budget
    /// keeps its scratch, so encoding it again reuses the buffers instead of
    /// regrowing them (the `large` load of `benches/proto_codec.rs`).
    #[test]
    fn encoder_scratch_keeps_the_capacity_of_an_event_within_the_budget() {
        std::thread::spawn(|| {
            let members: Vec<String> = (0..500)
                .map(|i| {
                    format!(
                        r#""key{i}":{{"value":{i},"label":"label {i}","flags":[true,false,null],"ratio":0.{i}}}"#
                    )
                })
                .collect();
            let json = format!(
                r#"{{"type":"STATE_SNAPSHOT","snapshot":{{{}}}}}"#,
                members.join(",")
            );
            let mut out = Vec::new();
            encode_json(json.as_bytes(), &mut out).unwrap();
            let kept = ENCODER.with(|e| e.borrow().max_capacity());
            assert!(
                kept > super::super::encoder::SCRATCH_KEEP_ENTRIES,
                "scratch {kept}"
            );
            out.clear();
            encode_json(json.as_bytes(), &mut out).unwrap();
            ENCODER.with(|e| assert_eq!(e.borrow().max_capacity(), kept));
        })
        .join()
        .unwrap();
    }

    /// The 256 KiB scratch bound in values, as the module documents it: a
    /// flat array of 6,000 numbers keeps its scratch, one of 10,000 does not.
    #[test]
    fn encoder_scratch_bound_is_a_few_thousand_values() {
        std::thread::spawn(|| {
            let encode = |n: usize| {
                let items: Vec<String> = (0..n).map(|i| i.to_string()).collect();
                let json = format!(
                    r#"{{"type":"STATE_SNAPSHOT","snapshot":[{}]}}"#,
                    items.join(",")
                );
                ENCODER.with(|e| *e.borrow_mut() = Encoder::new());
                encode_json(json.as_bytes(), &mut Vec::new()).unwrap();
                ENCODER.with(|e| e.borrow().max_capacity())
            };
            let keep = super::super::encoder::SCRATCH_KEEP_ENTRIES;
            assert!(encode(6_000) > keep, "6,000 values keep their scratch");
            assert!(encode(10_000) <= keep, "10,000 values release it");
        })
        .join()
        .unwrap();
    }

    /// The typed builder writes exactly the bytes of the text path.
    #[test]
    fn typed_events_encode_as_their_json_text_does() {
        let events = [
            json!({"type": "RUN_STARTED", "threadId": "t", "runId": "r", "input": {"threadId": "t", "runId": "r", "state": {"n": 1, "big": 18446744073709551615u64, "neg": -9007199254740993i64, "f": 0.1, "e": 1e300}, "messages": [{"id": "u", "role": "user", "content": "hi \"there\"\n\u{1F600}"}], "tools": [], "context": [], "forwardedProps": {"a": [null, true, false, {"b": []}]}}}),
            json!({"type": "TEXT_MESSAGE_CONTENT", "messageId": "m", "delta": "tab\tquote\"slash\\ctl\u{1}é"}),
            json!({"type": "TOOL_CALL_ARGS", "toolCallId": "c", "delta": "{\"q\":\"v\"}", "timestamp": 1700000000123i64}),
            json!({"type": "STATE_SNAPSHOT", "snapshot": {"2": "b", "1": "a", "x": {"10": 1, "9": 2}}}),
            json!({"type": "STATE_DELTA", "delta": [{"op": "replace", "path": "/a~1b", "value": null}, {"op": "move", "from": "/x", "path": "/y"}]}),
            json!({"type": "CUSTOM", "name": "n", "value": [1.5, -0.0, 2e-308, 123456789012345678u64]}),
            json!({"type": "RUN_FINISHED", "threadId": "t", "runId": "r", "result": {"ok": true}}),
            json!({"type": "RUN_ERROR", "message": "boom", "code": "E1"}),
            json!({"type": "TOOL_CALL_RESULT", "messageId": "tm", "toolCallId": "c", "content": "done", "role": "tool"}),
            json!({"type": "MESSAGES_SNAPSHOT", "messages": [{"id": "a", "role": "assistant", "content": "x", "toolCalls": [{"id": "c", "type": "function", "function": {"name": "f", "arguments": "{}"}}]}, {"id": "s", "role": "system", "content": "sys"}]}),
        ];
        for value in events {
            let event: Event = serde_json::from_value(value.clone()).unwrap();
            let (mut strings, mut scratch) =
                (String::new(), super::super::json::Scratch::default());
            assert!(
                super::super::json::build(&event, &mut strings, &mut scratch).is_some(),
                "the builder declined {value}"
            );
            let mut typed = vec![0xAA];
            encode_event(&event, &mut typed).unwrap();
            let mut text = vec![0xAA];
            encode_event_text(&event, &mut text).unwrap();
            assert_eq!(typed, text, "{value}");
        }
    }
}
