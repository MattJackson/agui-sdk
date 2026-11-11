//! Property tests for the protobuf codec: no input panics the decoder, the
//! framer or the encoder; decoding is invariant under re-chunking; open
//! payloads round-trip with JavaScript key order.

#![cfg(feature = "proto")]
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod proto_support;

use agui_sdk::proto::decode::{decode_event, decode_event_json, Decoded, DecodedJson};
use agui_sdk::proto::encode::{encode_json, encode_value};
use agui_sdk::proto::framing::{write_frame, FrameDecoder};
use agui_sdk::wire::Chunk;
use proptest::prelude::*;
use serde_json::{Map, Value};

/// The decoder's nesting bound: the pipeline's default `max_depth`.
const MAX_DEPTH: usize = 128;

fn corpus_bins() -> Vec<Vec<u8>> {
    proto_support::byte_cases()
        .map(|cases| cases.into_iter().map(|c| c.bin).collect())
        .unwrap_or_default()
}

fn depth(v: &Value) -> usize {
    match v {
        Value::Array(a) => 1 + a.iter().map(depth).max().unwrap_or(0),
        Value::Object(o) => 1 + o.values().map(depth).max().unwrap_or(0),
        _ => 0,
    }
}

/// Equality of JS values: numbers compare as the `f64` they denote, so an
/// integer beyond 2^53 spelled as digits equals the rounded float it reads
/// as in JS (`serde_json` would keep the two apart).
fn js_eq(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Number(x), Value::Number(y)) => x.as_f64() == y.as_f64(),
        (Value::Array(x), Value::Array(y)) => {
            x.len() == y.len() && x.iter().zip(y).all(|(p, q)| js_eq(p, q))
        }
        (Value::Object(x), Value::Object(y)) => {
            x.len() == y.len() && x.iter().all(|(k, v)| y.get(k).is_some_and(|w| js_eq(v, w)))
        }
        _ => a == b,
    }
}

/// Decodes `bytes` both ways; they must agree, and whatever decodes must be
/// bounded and re-encodable without panicking.
fn check_decode(bytes: &[u8]) {
    let value = decode_event(bytes);
    let mut text = String::new();
    let json = decode_event_json(bytes, &mut text);
    assert_eq!(value.is_ok(), json.is_ok());
    match (value, json) {
        (Ok(Decoded::Event(v)), Ok(DecodedJson::Event(t))) => {
            assert_eq!(v["type"], t);
            let parsed = serde_json::from_str::<Value>(&text).unwrap();
            assert!(js_eq(&parsed, &v), "{parsed} vs {v}");
            assert!(depth(&v) <= MAX_DEPTH);
            // May legitimately fail (an int64 beyond the safe range), never panic.
            let _ = encode_value(&v, &mut Vec::new());
            let _ = encode_json(text.as_bytes(), &mut Vec::new());
        }
        (
            Ok(Decoded::UnknownArm { field_number: a }),
            Ok(DecodedJson::UnknownArm { field_number: b }),
        ) => {
            assert_eq!(a, b);
            assert!(text.is_empty());
        }
        (Err(_), Err(_)) => assert!(text.is_empty()),
        (a, b) => panic!("decoders disagree: {a:?} vs {b:?}"),
    }
}

/// Every frame of `body` delivered in chunks of `sizes` (cycled), or the
/// first error.
fn deframe(body: &[u8], sizes: &[usize], max: usize) -> Result<(Vec<Chunk>, bool), String> {
    let mut d = FrameDecoder::new(max);
    let mut got = Vec::new();
    let mut at = 0;
    let mut i = 0;
    while at < body.len() {
        let n = sizes[i % sizes.len()].max(1).min(body.len() - at);
        d.push(Chunk::copy_from_slice(&body[at..at + n]));
        at += n;
        i += 1;
        while let Some(f) = d.next_frame().map_err(|e| e.to_string())? {
            got.push(f);
        }
    }
    Ok((got, d.has_partial()))
}

fn json_value() -> impl Strategy<Value = Value> {
    let leaf = prop_oneof![
        Just(Value::Null),
        any::<bool>().prop_map(Value::Bool),
        (-1_000_000i64..1_000_000).prop_map(Value::from),
        any::<f64>()
            // -0 decodes as 0, as JSON.stringify prints it.
            .prop_filter("finite, not -0", |x| x.is_finite()
                && !(*x == 0.0 && x.is_sign_negative()))
            .prop_map(Value::from),
        "[a-z0-9\\\\\"\u{e9}\u{1F600}\n]{0,6}".prop_map(Value::String),
    ];
    leaf.prop_recursive(6, 64, 6, |inner| {
        prop_oneof![
            prop::collection::vec(inner.clone(), 0..6).prop_map(Value::Array),
            prop::collection::vec(("[a-c0-9]{0,3}", inner), 0..6).prop_map(|kv| {
                let mut m = Map::new();
                for (k, v) in kv {
                    m.insert(k, v);
                }
                Value::Object(m)
            }),
        ]
    })
}

/// `v` as JavaScript would hold it: integral numbers in the safe range as
/// integers, object keys in own-property order.
fn js(v: &Value) -> Value {
    match v {
        Value::Array(a) => Value::Array(a.iter().map(js).collect()),
        Value::Object(o) => {
            let idx = |k: &str| -> Option<u32> {
                if k.is_empty() || k.len() > 10 || (k.len() > 1 && k.starts_with('0')) {
                    return None;
                }
                k.parse::<u64>()
                    .ok()
                    .filter(|n| *n < u64::from(u32::MAX))
                    .map(|n| n as u32)
            };
            let mut ints: Vec<_> = o
                .iter()
                .filter_map(|(k, v)| idx(k).map(|n| (n, k, v)))
                .collect();
            ints.sort_by_key(|e| e.0);
            let mut m = Map::new();
            for (_, k, v) in ints {
                m.insert(k.clone(), js(v));
            }
            for (k, v) in o {
                if idx(k).is_none() {
                    m.insert(k.clone(), js(v));
                }
            }
            Value::Object(m)
        }
        Value::Number(n) => {
            let x = n.as_f64().unwrap();
            if x.fract() == 0.0 && x.abs() <= 9_007_199_254_740_991.0 {
                Value::from(x as i64)
            } else {
                v.clone()
            }
        }
        _ => v.clone(),
    }
}

/// Equality that also compares object member order.
fn ordered_eq(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Object(x), Value::Object(y)) => {
            x.len() == y.len()
                && x.iter()
                    .zip(y.iter())
                    .all(|((ka, va), (kb, vb))| ka == kb && ordered_eq(va, vb))
        }
        (Value::Array(x), Value::Array(y)) => {
            x.len() == y.len() && x.iter().zip(y).all(|(p, q)| ordered_eq(p, q))
        }
        _ => a == b,
    }
}

/// Found by `mutated_corpus_never_panics`: an int64 timestamp beyond 2^53,
/// which the encoder refuses, so the frame is built by hand.
#[test]
fn decoders_agree_on_an_int64_beyond_the_safe_range() {
    fn varint(mut v: u64, out: &mut Vec<u8>) {
        while v >= 0x80 {
            out.push((v as u8) | 0x80);
            v >>= 7;
        }
        out.push(v as u8);
    }
    fn len_field(number: u64, body: &[u8], out: &mut Vec<u8>) {
        varint(number << 3 | 2, out);
        varint(body.len() as u64, out);
        out.extend_from_slice(body);
    }
    for ts in [47_850_746_040_811_521_i64, -47_287_796_087_390_210] {
        // BaseEvent { timestamp = 2 }
        let mut base = Vec::new();
        varint(2 << 3, &mut base);
        varint(ts as u64, &mut base);
        // TextMessageEndEvent { base_event = 1, message_id = 2 }
        let mut end = Vec::new();
        len_field(1, &base, &mut end);
        len_field(2, b"m1", &mut end);
        // Event { text_message_end = 3 }
        let mut event = Vec::new();
        len_field(3, &end, &mut event);
        check_decode(&event);
    }
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 1024, ..ProptestConfig::default() })]

    #[test]
    fn arbitrary_bytes_never_panic(
        bytes in prop::collection::vec(any::<u8>(), 0..512),
        sizes in prop::collection::vec(1usize..64, 1..8),
    ) {
        check_decode(&bytes);
        if let Ok((frames, _)) = deframe(&bytes, &sizes, 1 << 16) {
            for f in frames {
                check_decode(&f);
            }
        }
    }

    #[test]
    fn mutated_corpus_never_panics(
        pick in any::<prop::sample::Index>(),
        edits in prop::collection::vec((any::<prop::sample::Index>(), any::<u8>()), 1..6),
        cut in any::<prop::sample::Index>(),
    ) {
        let bins = corpus_bins();
        if bins.is_empty() {
            return Ok(());
        }
        let mut b = pick.get(&bins).clone();
        for (at, byte) in edits {
            let i = at.index(b.len().max(1));
            if let Some(slot) = b.get_mut(i) {
                *slot = byte;
            }
        }
        check_decode(&b);
        let n = cut.index(b.len() + 1);
        check_decode(&b[..n]);
    }

    #[test]
    fn arbitrary_text_never_panics(s in "\\PC{0,64}") {
        let _ = encode_json(s.as_bytes(), &mut Vec::new());
        let wrapped = format!(r#"{{"type":"CUSTOM","name":"n","value":{s}}}"#);
        let _ = encode_json(wrapped.as_bytes(), &mut Vec::new());
    }

    #[test]
    fn deframing_is_invariant_under_rechunking(
        payloads in prop::collection::vec(prop::collection::vec(any::<u8>(), 0..200), 0..8),
        sizes in prop::collection::vec(1usize..300, 1..6),
        cut in 0usize..4,
    ) {
        let mut body = Vec::new();
        for p in &payloads {
            write_frame(p, &mut body).unwrap();
        }
        let (got, partial) = deframe(&body, &sizes, 1 << 16).unwrap();
        prop_assert!(!partial);
        let got: Vec<&[u8]> = got.iter().map(|b| &b[..]).collect();
        let want: Vec<&[u8]> = payloads.iter().map(|p| &p[..]).collect();
        prop_assert_eq!(got, want);
        // Cutting the body short is truncation whenever a frame is cut.
        if cut > 0 && !body.is_empty() {
            let short = &body[..body.len() - cut.min(body.len())];
            let (_, partial) = deframe(short, &sizes, 1 << 16).unwrap();
            prop_assert!(partial || short.is_empty());
        }
    }

    #[test]
    fn open_payloads_round_trip(v in json_value()) {
        let ev = serde_json::json!({"type": "CUSTOM", "name": "n", "value": v, "metadata": {"m": v}});
        let mut bytes = Vec::new();
        encode_value(&ev, &mut bytes).unwrap();
        let text = serde_json::to_string(&ev).unwrap();
        let mut from_text = Vec::new();
        encode_json(text.as_bytes(), &mut from_text).unwrap();
        prop_assert_eq!(&from_text, &bytes);
        let Ok(Decoded::Event(got)) = decode_event(&bytes) else {
            panic!("does not decode")
        };
        let want = js(&v);
        prop_assert_eq!(&got["value"], &want);
        prop_assert_eq!(&got["metadata"]["m"], &want);
        // The text decoder keeps wire order in every build.
        let mut wire = String::new();
        decode_event_json(&bytes, &mut wire).unwrap();
        let mut again = Vec::new();
        encode_json(wire.as_bytes(), &mut again).unwrap();
        prop_assert_eq!(&again, &bytes);
        if cfg!(feature = "preserve-order") {
            prop_assert!(ordered_eq(&got["value"], &want), "{} vs {}", got["value"], want);
            let mut re = Vec::new();
            encode_value(&got, &mut re).unwrap();
            prop_assert_eq!(re, bytes);
        }
    }
}
