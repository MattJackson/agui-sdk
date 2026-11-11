//! Value lane: every upstream value document (spec/1.0/fixtures, discovered
//! from the spec cache). TEST-PLAN §3.
//!
//! * Gate A: `validate_strict` accepts every valid document and rejects every
//!   invalid one.
//! * Gate B: `enforce_document` accepts every valid document unchanged and
//!   silently. An invalid document is stripped or rejected according to
//!   what made it invalid, read from its `.expect.json` (the schema keyword
//!   that rejects it), never from a hand-written list:
//!   - unrecognised material only (`unevaluatedProperties`, `enum`: unknown
//!     members, roles, part kinds, patch ops, closed-set values) must be
//!     stripped, with a diagnostic, never rejected (PROC-02, PROC-03, D3);
//!   - an unknown union member (`oneOf`, `const`) may be stripped the same way;
//!   - anything else is malformed and must be rejected (PROC-04).
//!
//!   A stripped result is strictly valid, or `null` when the document's root
//!   itself was the unrecognised material (its container is dropped).
//! * Round trip: every valid event document parses into `Event` and
//!   re-serializes to an equal JSON value.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use serde_json::Value;

use agui_sdk::diagnostics::{CollectingSink, Reporter};
use agui_sdk::pipeline::enforce::{enforce_document, validate_strict};
use agui_sdk::Event;
use agui_test_support::corpus::{self, ValueDocument};
use agui_test_support::lane::check_each;

fn lane(name: &str, check: impl Fn(&ValueDocument) -> Result<Option<String>, String>) {
    let Some(docs) = corpus::values() else { return };
    check_each(name, &docs, |d| d.relative.clone(), check);
}

#[test]
fn gate_a_strict() {
    lane("values/strict", |d| {
        match (d.valid, validate_strict(&d.anchor, &d.bytes)) {
            (true, Ok(())) | (false, Err(_)) => Ok(None),
            (true, Err(e)) => Err(format!("valid document rejected: {e}")),
            (false, Ok(())) => Err("invalid document accepted".to_owned()),
        }
    });
}

/// Schema keywords whose failure means "unrecognised material": the
/// consumer strips it (PROC-02, PROC-03, D3).
const UNRECOGNISED: &[&str] = &["unevaluatedProperties", "enum"];
/// Keywords that may also be unrecognised material: an unknown union member.
const MAYBE_UNRECOGNISED: &[&str] = &["oneOf", "const"];

#[test]
fn gate_b_consumer_pipeline() {
    let stripped = AtomicUsize::new(0);
    let rejected = AtomicUsize::new(0);
    lane("values/enforce", |d| {
        let input: Value =
            serde_json::from_slice(&d.bytes).map_err(|e| format!("not JSON: {e}"))?;
        let sink = Arc::new(CollectingSink::new());
        let mut reporter = Reporter::new(sink.clone(), 256);
        let result = enforce_document(&d.anchor, input.clone(), &mut reporter);
        let diagnostics = sink.take();
        let keyword = d
            .expect
            .as_ref()
            .and_then(|e| e.get("keyword"))
            .and_then(Value::as_str)
            .unwrap_or("");
        match (d.valid, result) {
            (true, Ok(out)) if out == input && diagnostics.is_empty() => Ok(None),
            (true, Ok(out)) => Err(format!(
                "valid document changed or warned: {out} / {diagnostics:?}"
            )),
            (true, Err(e)) => Err(format!("valid document rejected: {e}")),
            (false, Err(e)) => {
                rejected.fetch_add(1, Ordering::Relaxed);
                if UNRECOGNISED.contains(&keyword) {
                    Err(format!(
                        "only unrecognised material ({keyword}), but rejected: {e}"
                    ))
                } else {
                    Ok(None)
                }
            }
            (false, Ok(out)) => {
                stripped.fetch_add(1, Ordering::Relaxed);
                if !UNRECOGNISED.contains(&keyword) && !MAYBE_UNRECOGNISED.contains(&keyword) {
                    return Err(format!("malformed ({keyword:?}), but accepted"));
                }
                if diagnostics.is_empty() {
                    return Err("stripped without a diagnostic".to_owned());
                }
                if out.is_null() {
                    // The root itself was unrecognised: dropped whole.
                    return Ok(None);
                }
                let bytes = serde_json::to_vec(&out).map_err(|e| e.to_string())?;
                validate_strict(&d.anchor, &bytes)
                    .map(|()| None)
                    .map_err(|e| format!("stripped, but the result is still invalid: {e}"))
            }
        }
    });
    eprintln!(
        "values/enforce: {} invalid documents stripped, {} rejected",
        stripped.load(Ordering::Relaxed),
        rejected.load(Ordering::Relaxed)
    );
}

#[test]
fn valid_events_round_trip() {
    lane("values/round-trip", |d| {
        if !d.valid || !d.anchor.ends_with("Event") {
            return Ok(None);
        }
        let input: Value =
            serde_json::from_slice(&d.bytes).map_err(|e| format!("not JSON: {e}"))?;
        let event: Event =
            serde_json::from_slice(&d.bytes).map_err(|e| format!("does not parse: {e}"))?;
        let back = serde_json::to_value(&event).map_err(|e| e.to_string())?;
        if back == input {
            Ok(None)
        } else {
            Err(format!("re-serialized as {back}"))
        }
    });
}
