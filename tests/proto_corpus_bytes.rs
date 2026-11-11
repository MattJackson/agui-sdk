//! Protobuf byte lane (TEST-PLAN §3): the upstream TypeScript byte corpus,
//! read from the spec cache. Every `<Anchor>__<name>.json.bin` is the
//! canonical encoding of `spec/1.0/fixtures/<Anchor>/valid/<name>.json`.
//!
//! * JSON text path, every build: `encode_json(json)` is byte-identical to the
//!   `.bin` (PB-12), and `decode_event_json(bin)` re-encodes to it.
//! * `decode_event(bin)` is semantically equal to the fixture, with the
//!   documented empty-materialisation exception (PROC-09).
//! * `Value` and typed paths: semantically equal in every build; byte-identical
//!   with the `preserve-order` feature (D4).

#![cfg(feature = "proto")]
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod proto_support;

use agui_sdk::proto::decode::{decode_event, decode_event_json, Decoded, DecodedJson};
use agui_sdk::proto::encode::{encode_event, encode_json, encode_value};
use agui_sdk::Event;
use proto_support::{byte_cases, sem_eq, ByteCase};
use serde_json::{json, Value};

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn decoded(case: &ByteCase) -> Value {
    match decode_event(&case.bin) {
        Ok(Decoded::Event(v)) => v,
        other => panic!("{}: decode_event gave {other:?}", case.name),
    }
}

/// Runs `check` over every case and reports all failures at once.
fn each(check: impl Fn(&ByteCase) -> Result<(), String>) {
    let Some(cases) = byte_cases() else {
        return;
    };
    let failures: Vec<String> = cases.iter().filter_map(|c| check(c).err()).collect();
    assert!(
        failures.is_empty(),
        "{} of {} cases failed:\n{}",
        failures.len(),
        cases.len(),
        failures.join("\n")
    );
}

#[test]
fn encode_json_reproduces_every_corpus_file() {
    each(|c| {
        let mut out = Vec::new();
        encode_json(&c.json, &mut out).map_err(|e| format!("{}: {e}", c.name))?;
        if out == c.bin {
            Ok(())
        } else {
            Err(format!(
                "{}:\n  got  {}\n  want {}",
                c.name,
                hex(&out),
                hex(&c.bin)
            ))
        }
    });
}

#[test]
fn decode_event_matches_every_fixture() {
    each(|c| {
        let want: Value = serde_json::from_slice(&c.json).unwrap();
        let got = decoded(c);
        if sem_eq(&want, &got) {
            Ok(())
        } else {
            Err(format!("{}:\n  want {want}\n  got  {got}", c.name))
        }
    });
}

#[test]
fn decode_event_json_round_trips_bytes_and_agrees_with_decode_event() {
    each(|c| {
        let mut text = String::new();
        let kind = decode_event_json(&c.bin, &mut text).map_err(|e| format!("{}: {e}", c.name))?;
        let value = decoded(c);
        if !matches!(kind, DecodedJson::Event(t) if value["type"] == t) {
            return Err(format!("{}: decode_event_json gave {kind:?}", c.name));
        }
        if serde_json::from_str::<Value>(&text).ok().as_ref() != Some(&value) {
            return Err(format!("{}: text and value decoders disagree", c.name));
        }
        let mut again = Vec::new();
        encode_json(text.as_bytes(), &mut again).map_err(|e| format!("{}: {e}", c.name))?;
        if again != c.bin {
            return Err(format!("{}: decoded text does not re-encode", c.name));
        }
        // With preserve-order the value keeps wire order too, so its text is
        // identical to the streaming decoder's.
        if cfg!(feature = "preserve-order") && serde_json::to_string(&value).unwrap() != text {
            return Err(format!("{}: value order differs from wire order", c.name));
        }
        Ok(())
    });
}

/// Without `preserve-order` the maps are sorted, so the bytes may differ in
/// open-object order but must decode to the same event.
#[test]
fn encode_value_reproduces_the_corpus_semantically_and_with_preserve_order_exactly() {
    each(|c| {
        let want: Value = serde_json::from_slice(&c.json).unwrap();
        for (what, input) in [("fixture", want.clone()), ("decoded", decoded(c))] {
            let mut out = Vec::new();
            encode_value(&input, &mut out).map_err(|e| format!("{}: {what}: {e}", c.name))?;
            if cfg!(feature = "preserve-order") && out != c.bin {
                return Err(format!("{}: {what}: bytes differ", c.name));
            }
            let Ok(Decoded::Event(back)) = decode_event(&out) else {
                return Err(format!("{}: {what}: output does not decode", c.name));
            };
            if !sem_eq(&want, &back) {
                return Err(format!("{}: {what}: {back} differs from {want}", c.name));
            }
        }
        Ok(())
    });
}

/// The typed path: JSON → `Event` → `encode_event` → `decode_event`.
#[test]
fn encode_event_round_trips_every_event_fixture() {
    each(|c| {
        let want: Value = serde_json::from_slice(&c.json).unwrap();
        // The byte corpus pins only events, so a case that does not parse
        // as one is a failure, never a silent skip.
        let event = serde_json::from_slice::<Event>(&c.json)
            .map_err(|e| format!("{}: not an Event: {e}", c.name))?;
        let mut out = Vec::new();
        encode_event(&event, &mut out).map_err(|e| format!("{}: {e}", c.name))?;
        if cfg!(feature = "preserve-order") && out != c.bin {
            return Err(format!(
                "{}: typed bytes differ:\n  got  {}\n  want {}",
                c.name,
                hex(&out),
                hex(&c.bin)
            ));
        }
        let Ok(Decoded::Event(back)) = decode_event(&out) else {
            return Err(format!("{}: typed output does not decode", c.name));
        };
        if sem_eq(&want, &back) {
            Ok(())
        } else {
            Err(format!("{}: typed:\n  want {want}\n  got  {back}", c.name))
        }
    });
}

/// The implicit-presence slack of `sem_eq` stops at open-JSON members: inside
/// them `""`, `[]` and a present key are explicit on the wire.
#[test]
fn sem_eq_is_strict_inside_open_payloads() {
    assert!(!sem_eq(
        &json!({"type": "STATE_SNAPSHOT", "snapshot": {"a": []}}),
        &json!({"type": "STATE_SNAPSHOT", "snapshot": {}})
    ));
    assert!(!sem_eq(
        &json!({"type": "CUSTOM", "name": "n", "value": {"s": ""}}),
        &json!({"type": "CUSTOM", "name": "n", "value": {}})
    ));
    assert!(!sem_eq(
        &json!({"type": "RUN_STARTED", "metadata": {"k": ""}}),
        &json!({"type": "RUN_STARTED", "metadata": {}})
    ));
    // The open member's own key is explicit too (Value has presence).
    assert!(!sem_eq(
        &json!({"type": "CUSTOM", "name": "n", "value": ""}),
        &json!({"type": "CUSTOM", "name": "n"})
    ));
    assert!(!sem_eq(
        &json!({"type": "CUSTOM", "name": "n"}),
        &json!({"type": "CUSTOM", "name": "n", "value": ""})
    ));
    assert!(!sem_eq(
        &json!({"type": "STATE_SNAPSHOT", "snapshot": []}),
        &json!({"type": "STATE_SNAPSHOT"})
    ));
    assert!(!sem_eq(
        &json!({"type": "STATE_DELTA", "delta": [{"op": "replace", "path": "/a", "value": ""}]}),
        &json!({"type": "STATE_DELTA", "delta": [{"op": "replace", "path": "/a"}]})
    ));
    // Protocol-level fields keep the slack.
    assert!(sem_eq(
        &json!({"type": "TEXT_MESSAGE_CONTENT", "messageId": "m", "delta": ""}),
        &json!({"type": "TEXT_MESSAGE_CONTENT", "messageId": "m"})
    ));
    // `value` is a plain string (implicit presence) outside CUSTOM / patch ops.
    assert!(sem_eq(
        &json!({"type": "RUN_STARTED", "input": {"context": [{"description": "d", "value": ""}]}}),
        &json!({"type": "RUN_STARTED", "input": {"context": [{"description": "d"}]}})
    ));
    assert!(sem_eq(
        &json!({"type": "RUN_STARTED", "input": {"context": [{"description": "d"}]}}),
        &json!({"type": "RUN_STARTED", "input": {"context": [{"description": "d", "value": ""}]}})
    ));
    // Numbers still compare as f64.
    assert!(sem_eq(
        &json!({"snapshot": {"n": 1}}),
        &json!({"snapshot": {"n": 1.0}})
    ));
}
