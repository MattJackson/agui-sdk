//! The outgoing run input, from typed value to
//! [`TransportRequest::body`](super::TransportRequest::body), in the order the compatibility layer
//! requires (docs/API.md §4, "Outgoing input"):
//!
//! 1. [`Boundary::upgrade_input`]: retired shapes (legacy binary parts,
//!    `null` in optional positions) become 1.0 shapes, for every peer;
//! 2. [`EraShims::downgrade_input`], only for a pinned pre-versioning peer:
//!    1.0 shapes become that era's (`protocolVersion` removed, RUNIN-08;
//!    parts flattened for 0.0.39). Downgrading first would hand the shims a
//!    shape they do not know;
//! 3. enforcement: unknown material stripped with warnings, malformed known
//!    fields refused before anything is sent (PROC-32..PROC-34).
//!
//! Middleware, when installed, sees the input between steps 2 and 3
//! ([`prepare_value`] then [`finish_value`]). With neither middleware nor a
//! pinned era, [`encode_input`] checks the typed input and serializes it once:
//! a typed `RunAgentInput` cannot hold a retired shape, so step 1 is the
//! identity there and is skipped with no `Value` round trip.

use serde_json::Value;

use crate::diagnostics::Reporter;
use crate::error::{Error, ErrorCode, Result};
use crate::pipeline::compat::{Boundary, EraShims};
use crate::pipeline::enforce::Enforcer;
use crate::run_input::RunAgentInput;
use crate::wire::Chunk;

fn json_error(error: serde_json::Error) -> Error {
    Error::new(
        ErrorCode::InvalidRequest,
        "the run input could not be serialized",
    )
    .with_source(error)
}

/// Steps 1 and 2 on an input already in JSON form.
pub(crate) fn prepare_value(
    mut input: Value,
    shims: Option<&EraShims>,
    reporter: &mut Reporter,
) -> Value {
    Boundary::upgrade_input(&mut input, reporter);
    if let Some(shims) = shims {
        shims.downgrade_input(&mut input, reporter);
    }
    input
}

/// Step 3 and serialization, for an input in JSON form (after middleware).
pub(crate) fn finish_value(
    mut input: Value,
    enforcer: &Enforcer,
    reporter: &mut Reporter,
) -> Result<Chunk> {
    enforcer.enforce_outgoing_value(&mut input, reporter)?;
    serde_json::to_vec(&input)
        .map(Chunk::from)
        .map_err(json_error)
}

/// The whole sequence for a typed input with no middleware.
pub(crate) fn encode_input(
    input: &RunAgentInput,
    shims: Option<&EraShims>,
    enforcer: &Enforcer,
    reporter: &mut Reporter,
) -> Result<Chunk> {
    match shims {
        None => {
            enforcer.check_outgoing_input(input, reporter)?;
            serde_json::to_vec(input)
                .map(Chunk::from)
                .map_err(json_error)
        }
        Some(_) => {
            let value = serde_json::to_value(input).map_err(json_error)?;
            finish_value(prepare_value(value, shims, reporter), enforcer, reporter)
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use serde_json::json;

    use super::*;
    use crate::diagnostics::{CollectingSink, DiagnosticSink};
    use crate::pipeline::Limits;
    use crate::version::LegacyEra;

    fn reporter() -> (Arc<CollectingSink>, Reporter) {
        let sink = Arc::new(CollectingSink::new());
        let dynamic: Arc<dyn DiagnosticSink> = sink.clone();
        (sink, Reporter::new(dynamic, 256))
    }

    fn base() -> Value {
        json!({
            "threadId": "t", "runId": "r", "state": {}, "tools": [], "context": [],
            "forwardedProps": {}, "protocolVersion": "1.0",
            "messages": [{"id": "u1", "role": "user", "content": [
                {"type": "text", "text": "look"},
                {"type": "binary", "mimeType": "image/png", "data": "iVBORw0KGgo="}
            ]}]
        })
    }

    #[test]
    fn upgrades_before_downgrading_before_enforcing() {
        let (sink, mut reporter) = reporter();
        let enforcer = Enforcer::new(Limits::default());
        // A legacy binary part reaches enforcement as a 1.0 media part.
        let current = prepare_value(base(), None, &mut reporter);
        let parts = &current["messages"][0]["content"];
        assert_eq!(parts[1]["type"], json!("image"), "{parts}");
        let body =
            finish_value(current, &enforcer, &mut reporter).unwrap_or_else(|e| panic!("{e}"));
        let sent: Value = serde_json::from_slice(&body).unwrap_or_default();
        assert_eq!(sent["protocolVersion"], json!("1.0"));

        // A 0.0.39 peer: the upgraded parts are flattened to text and the
        // version is removed; the legacy part never reaches the shims raw.
        let shims = EraShims::new(LegacyEra::V0_0_39);
        let old = prepare_value(base(), Some(&shims), &mut reporter);
        assert_eq!(old["messages"][0]["content"], json!("look"));
        assert!(old.get("protocolVersion").is_none());
        let body = finish_value(old, &enforcer, &mut reporter).unwrap_or_else(|e| panic!("{e}"));
        let sent: Value = serde_json::from_slice(&body).unwrap_or_default();
        assert_eq!(sent["messages"][0]["content"], json!("look"));
        // The shims saw the upgraded part, not the legacy one.
        let notes: Vec<String> = sink.take().into_iter().map(|d| d.message).collect();
        assert_eq!(notes.len(), 1, "{notes:?}");
        assert!(notes[0].contains("an image part was dropped"), "{notes:?}");
    }

    #[test]
    fn typed_inputs_take_the_single_serialization_path() {
        let (_, mut reporter) = reporter();
        let enforcer = Enforcer::new(Limits::default());
        let mut value = base();
        value["messages"] = json!([{"id": "u1", "role": "user", "content": "hi"}]);
        let input: RunAgentInput =
            serde_json::from_value(value.clone()).unwrap_or_else(|e| panic!("{e}"));
        let body =
            encode_input(&input, None, &enforcer, &mut reporter).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(serde_json::from_slice::<Value>(&body).ok(), Some(value));
        let shims = EraShims::new(LegacyEra::V0_0_57);
        let body = encode_input(&input, Some(&shims), &enforcer, &mut reporter)
            .unwrap_or_else(|e| panic!("{e}"));
        let sent: Value = serde_json::from_slice(&body).unwrap_or_default();
        assert!(sent.get("protocolVersion").is_none(), "RUNIN-08");
    }

    #[test]
    fn malformed_input_is_refused_before_sending() {
        let (_, mut reporter) = reporter();
        let enforcer = Enforcer::new(Limits::default());
        let mut bad = base();
        bad["messages"][0]["id"] = json!(42);
        let error = finish_value(
            prepare_value(bad, None, &mut reporter),
            &enforcer,
            &mut reporter,
        )
        .err()
        .map(|e| e.code());
        assert_eq!(error, Some(ErrorCode::InvalidRequest));
    }
}
