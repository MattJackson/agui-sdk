//! Protobuf byte lane: every case of the TypeScript byte corpus (discovered
//! from the spec cache) against its JSON source. TEST-PLAN §3.
//!
//! * `encode_json(json)` is byte-identical to the `.bin` (PB-12).
//! * `decode_event(bin)` is semantically equal to the JSON, except that a
//!   member the JSON omits may be materialised empty (`""`, `[]`, `{}`, `0`,
//!   `false`): the protobuf exception PROC-09 documents.
//! * Typed path: JSON → `Event` → `encode_event` → `decode_event` is
//!   semantically equal under the same rule.
//! * Framing: each case framed; the first 16 concatenated and re-split at
//!   every byte offset (PB-09).

use serde_json::Value;

use agui_sdk::proto::decode::{decode_event, Decoded};
use agui_sdk::proto::encode::{encode_event, encode_json};
use agui_sdk::proto::framing::{write_frame, FrameDecoder, DEFAULT_MAX_FRAME_BYTES};
use agui_sdk::wire::Chunk;
use agui_sdk::Event;
use agui_test_support::corpus::{self, ProtoCase};
use agui_test_support::lane::check_each;

fn is_empty_value(v: &Value) -> bool {
    match v {
        Value::String(s) => s.is_empty(),
        Value::Array(a) => a.is_empty(),
        Value::Object(o) => o.is_empty(),
        Value::Number(n) => n.as_f64() == Some(0.0),
        Value::Bool(b) => !b,
        Value::Null => false,
    }
}

/// Equal, allowing `got` extra members with empty values.
fn equivalent(want: &Value, got: &Value, at: &str) -> Result<(), String> {
    match (want, got) {
        (Value::Object(w), Value::Object(g)) => {
            for (k, wv) in w {
                let gv = g.get(k).ok_or_else(|| format!("{at}/{k}: missing"))?;
                equivalent(wv, gv, &format!("{at}/{k}"))?;
            }
            match g
                .iter()
                .find(|(k, v)| !w.contains_key(*k) && !is_empty_value(v))
            {
                Some((k, v)) => Err(format!("{at}/{k}: unexpected {v}")),
                None => Ok(()),
            }
        }
        (Value::Array(w), Value::Array(g)) if w.len() == g.len() => w
            .iter()
            .zip(g)
            .enumerate()
            .try_for_each(|(i, (a, b))| equivalent(a, b, &format!("{at}/{i}"))),
        (Value::Number(a), Value::Number(b)) if a.as_f64() == b.as_f64() => Ok(()),
        _ if want == got => Ok(()),
        _ => Err(format!("{at}: expected {want}, got {got}")),
    }
}

fn decoded(bytes: &[u8]) -> Result<Value, String> {
    match decode_event(bytes).map_err(|e| format!("decode: {e}"))? {
        Decoded::Event(v) => Ok(v),
        other => Err(format!("decoded as {other:?}")),
    }
}

fn lane(name: &str, check: impl Fn(&ProtoCase) -> Result<Option<String>, String>) {
    let Some(cases) = corpus::proto_cases() else {
        return;
    };
    check_each(name, &cases, |c| c.name.clone(), check);
}

#[test]
fn encode_json_is_byte_identical() {
    lane("proto/encode_json", |c| {
        let mut out = Vec::new();
        encode_json(&c.json, &mut out).map_err(|e| format!("encode: {e}"))?;
        if out == c.bytes {
            Ok(None)
        } else {
            Err(format!(
                "{} bytes differ from the {}-byte corpus encoding",
                out.len(),
                c.bytes.len()
            ))
        }
    });
}

#[test]
fn decode_matches_the_json() {
    lane("proto/decode", |c| {
        let want: Value = serde_json::from_slice(&c.json).map_err(|e| e.to_string())?;
        equivalent(&want, &decoded(&c.bytes)?, "").map(|()| None)
    });
}

#[test]
fn typed_path_round_trips() {
    lane("proto/typed", |c| {
        let want: Value = serde_json::from_slice(&c.json).map_err(|e| e.to_string())?;
        let event: Event = serde_json::from_slice(&c.json).map_err(|e| format!("parse: {e}"))?;
        let mut out = Vec::new();
        encode_event(&event, &mut out).map_err(|e| format!("encode: {e}"))?;
        equivalent(&want, &decoded(&out)?, "").map(|()| None)
    });
}

#[test]
fn framing_survives_every_split() {
    let Some(cases) = corpus::proto_cases() else {
        return;
    };
    let mut wire = Vec::new();
    let mut payloads = Vec::new();
    for c in cases.iter().take(16) {
        write_frame(&c.bytes, &mut wire).unwrap_or_else(|e| panic!("{}: {e}", c.name));
        payloads.push(c.bytes.clone());
    }
    for split in 0..=wire.len() {
        let mut decoder = FrameDecoder::new(DEFAULT_MAX_FRAME_BYTES);
        let mut got = Vec::new();
        for piece in [&wire[..split], &wire[split..]] {
            decoder.push(Chunk::copy_from_slice(piece));
            while let Some(frame) = decoder
                .next_frame()
                .unwrap_or_else(|e| panic!("split {split}: {e}"))
            {
                got.push(frame.to_vec());
            }
        }
        assert!(!decoder.has_partial(), "split {split}: partial frame left");
        assert_eq!(got, payloads, "split {split}");
    }
}
