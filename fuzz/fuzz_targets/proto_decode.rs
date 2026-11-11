//! decode_event over arbitrary payloads. Invariants: no panic; an `Ok` event
//! re-encodes and decodes to the same value, unless it carries an `int64`
//! outside the safe range, which the decoder hands on exactly and the encoder
//! refuses by design.
#![no_main]

use agui_sdk::proto::decode::{decode_event, Decoded};
use agui_sdk::proto::encode::encode_value;
use libfuzzer_sys::fuzz_target;
use serde_json::Value;

/// Whether `v` holds an integer beyond ±(2^53−1). Only an `int64` member can:
/// open JSON numbers come back as `f64` when not safe.
fn has_unsafe_int(v: &Value) -> bool {
    const MAX_SAFE: u64 = (1 << 53) - 1;
    match v {
        Value::Number(n) => {
            n.as_u64().is_some_and(|x| x > MAX_SAFE)
                || n.as_i64().is_some_and(|x| x.unsigned_abs() > MAX_SAFE)
        }
        Value::Array(a) => a.iter().any(has_unsafe_int),
        Value::Object(o) => o.values().any(has_unsafe_int),
        _ => false,
    }
}

fuzz_target!(|data: &[u8]| {
    if let Ok(Decoded::Event(value)) = decode_event(data) {
        let mut out = Vec::new();
        if has_unsafe_int(&value) {
            let err = encode_value(&value, &mut out)
                .expect_err("the encoder accepted an int64 outside the safe range");
            assert_eq!(err.code(), agui_sdk::ErrorCode::Encode, "{err}");
            return;
        }
        encode_value(&value, &mut out)
            .unwrap_or_else(|e| panic!("decoded value does not re-encode: {e}"));
        match decode_event(&out) {
            Ok(Decoded::Event(again)) => assert_eq!(again, value, "re-encode changed the value"),
            other => panic!("re-encoded value does not decode: {other:?}"),
        }
    }
});
