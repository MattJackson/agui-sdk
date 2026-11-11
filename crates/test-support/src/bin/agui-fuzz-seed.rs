//! Seeds the cargo-fuzz corpora (`fuzz/corpus/<target>/`) from the cached
//! upstream spec data, so every campaign starts from real protocol traffic.
//!
//! ```text
//! cargo xtask spec-fetch
//! cargo run -p agui-test-support --bin agui-fuzz-seed -- fuzz/corpus
//! ```
//!
//! Input formats match the targets in `fuzz/fuzz_targets/`:
//! byte streams for the decoders, single JSON documents for enforcement and
//! admission, `document \0 operations` for the patch target.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use serde_json::{json, Value};

use agui_sdk::patch::{apply, PatchBudget, PatchLimits, PatchOperation};
use agui_test_support::corpus;
use agui_test_support::mock::sse_body;

struct Seeder {
    out: PathBuf,
    written: usize,
}

impl Seeder {
    fn put(&mut self, target: &str, name: &str, bytes: &[u8]) -> std::io::Result<()> {
        let dir = self.out.join(target);
        std::fs::create_dir_all(&dir)?;
        let safe: String = name
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                    c
                } else {
                    '_'
                }
            })
            .collect();
        std::fs::write(dir.join(format!("seed-{safe}")), bytes)?;
        self.written += 1;
        Ok(())
    }
}

fn frame(payload: &[u8]) -> Vec<u8> {
    // PB-08: 4-byte big-endian length, then the payload.
    let mut out = (payload.len() as u32).to_be_bytes().to_vec();
    out.extend_from_slice(payload);
    out
}

fn to_vec(v: &Value) -> Vec<u8> {
    serde_json::to_vec(v).unwrap_or_default()
}

/// `document \0 operations` seeds for every `STATE_DELTA` in a stream, keyed
/// by event index. The document is the one the delta actually applies to:
/// the latest snapshot, advanced by each earlier delta that applied.
fn patch_seeds(stream: &[Value]) -> Vec<(usize, Vec<u8>)> {
    let mut state = json!({});
    let mut out = Vec::new();
    for (i, event) in stream.iter().enumerate() {
        match event.get("type").and_then(Value::as_str) {
            Some("STATE_SNAPSHOT") => state = event.get("snapshot").cloned().unwrap_or(Value::Null),
            Some("STATE_DELTA") => {
                let delta = event.get("delta").unwrap_or(&Value::Null);
                let mut input = to_vec(&state);
                input.push(0);
                input.extend(to_vec(delta));
                out.push((i, input));
                // `apply` is atomic: a delta that fails to parse or apply
                // leaves `state` as it was.
                if let Ok(ops) = serde_json::from_value::<Vec<PatchOperation>>(delta.clone()) {
                    let mut budget = PatchBudget::for_document(&state, PatchLimits::default());
                    let _ = apply(&mut state, &ops, &mut budget);
                }
            }
            _ => {}
        }
    }
    out
}

fn seed(out: &Path) -> std::io::Result<usize> {
    let mut s = Seeder {
        out: out.to_path_buf(),
        written: 0,
    };
    let streams = corpus::streams().unwrap_or_default();
    let values = corpus::values().unwrap_or_default();
    let protos = corpus::proto_cases().unwrap_or_default();
    if streams.is_empty() && values.is_empty() && protos.is_empty() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "no spec cache: run 'cargo xtask spec-fetch' first",
        ));
    }

    for f in &streams {
        let sse = sse_body(f.stream());
        s.put("sse_decoder", &f.name, &sse)?;
        s.put("pipeline_json", &f.name, &sse)?;
        for (i, event) in f.stream().iter().enumerate() {
            s.put("enforce_event", &format!("{}-{i}", f.name), &to_vec(event))?;
        }
        for (i, bytes) in patch_seeds(f.stream()) {
            s.put("patch_apply", &format!("{}-{i}", f.name), &bytes)?;
        }
    }
    for d in &values {
        let name = d.relative.trim_end_matches(".json").replace('/', "-");
        if d.anchor.ends_with("Event") {
            s.put("enforce_event", &name, &d.bytes)?;
        }
        if d.anchor == "RunAgentInput" {
            s.put("admit_input", &name, &d.bytes)?;
        }
        if d.anchor == "StateDeltaEvent" || d.anchor == "ActivityDeltaEvent" {
            if let Ok(v) = serde_json::from_slice::<Value>(&d.bytes) {
                if let Some(ops) = v.get("delta").or_else(|| v.get("patch")) {
                    let mut input = b"{}".to_vec();
                    input.push(0);
                    input.extend(to_vec(ops));
                    s.put("patch_apply", &name, &input)?;
                }
            }
        }
        if d.anchor == "Interrupt" {
            if let Some(at) = serde_json::from_slice::<Value>(&d.bytes)
                .ok()
                .and_then(|v| {
                    v.get("expiresAt")
                        .and_then(Value::as_str)
                        .map(str::to_owned)
                })
            {
                s.put("expiry_parse", &name, at.as_bytes())?;
            }
        }
    }
    for p in &protos {
        s.put("proto_decode", &p.name, &p.bytes)?;
        s.put("proto_frames", &p.name, &frame(&p.bytes))?;
    }
    for (i, at) in [
        "2026-11-10T12:00:00Z",
        "2026-11-10T12:00:00.123+02:00",
        "1970-01-01T00:00:00Z",
        "not a date",
    ]
    .iter()
    .enumerate()
    {
        s.put("expiry_parse", &format!("sample-{i}"), at.as_bytes())?;
    }
    for v in [
        "1.0",
        "1.1",
        "2.0",
        "0.0.39",
        "0.0.45",
        "0.0.57",
        "1.0.0-rc.1",
        "",
        "x",
    ] {
        s.put("version_parse", &format!("v{v}"), v.as_bytes())?;
    }
    Ok(s.written)
}

fn main() -> ExitCode {
    let out = std::env::args()
        .nth(1)
        .map_or_else(|| PathBuf::from("fuzz/corpus"), PathBuf::from);
    match seed(&out) {
        Ok(n) => {
            println!("wrote {n} seed inputs under {}", out.display());
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("agui-fuzz-seed: {e}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn later_deltas_are_seeded_against_the_applied_state() {
        let stream = vec![
            json!({"type": "STATE_SNAPSHOT", "snapshot": {}}),
            json!({"type": "STATE_DELTA", "delta": [{"op": "add", "path": "/a", "value": 1}]}),
            json!({"type": "STATE_DELTA", "delta": [{"op": "replace", "path": "/a", "value": 2}]}),
        ];
        let seeds = patch_seeds(&stream);
        assert_eq!(seeds.len(), 2);
        let mut docs = Vec::new();
        for (_, bytes) in &seeds {
            let nul = bytes.iter().position(|b| *b == 0).expect("NUL separator");
            let mut doc: Value = serde_json::from_slice(&bytes[..nul]).expect("document");
            let ops: Vec<PatchOperation> =
                serde_json::from_slice(&bytes[nul + 1..]).expect("operations");
            docs.push(doc.clone());
            let mut budget = PatchBudget::for_document(&doc, PatchLimits::default());
            apply(&mut doc, &ops, &mut budget).expect("seed must exercise the success path");
        }
        assert_eq!(docs[1], json!({"a": 1}));
    }
}
