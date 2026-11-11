//! Shared helpers for the protobuf tests: where the upstream protobuf data
//! lives, and the byte corpus.
//!
//! Also compiled into the crate's unit tests (as `crate::test_spec`, via
//! `#[path]` in `src/lib.rs`), so it uses nothing but `std` and
//! `serde_json`.
//!
//! Nothing here is vendored (D6): the `.proto` sources, `proto-freeze.txt`,
//! the byte corpus and the value fixtures are read from the spec cache that
//! `cargo xtask spec-fetch` fills, pinned by `spec.lock`.

#![allow(dead_code, clippy::unwrap_used, clippy::expect_used)]

use std::path::PathBuf;

/// The workspace root (this crate's manifest directory).
fn workspace() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

/// The commit `spec.lock` pins.
fn pinned_commit() -> String {
    let lock = std::fs::read_to_string(workspace().join("spec.lock")).expect("read spec.lock");
    lock.lines()
        .find_map(|l| {
            let (k, v) = l.split_once('=')?;
            (k.trim() == "commit").then(|| v.trim().trim_matches('"').to_owned())
        })
        .expect("spec.lock has a commit")
}

/// Root of the extracted upstream tree for the pinned commit, or `None`
/// (after printing why) when it is not available; the calling test then
/// returns early. The same rules as `agui_test_support::spec::root`, in std
/// only (this file is also compiled into the crate's unit tests):
/// `$AGUI_SPEC_DIR`, else `<workspace>/.spec-cache/<commit>`, and either way
/// its `.commit` stamp must name the commit `spec.lock` pins.
/// `AGUI_REQUIRE_SPEC=1` turns a missing cache into a failure; a stale cache
/// always fails. Nothing outside the pinned cache is ever read.
pub fn root() -> Option<PathBuf> {
    let commit = pinned_commit();
    let cache = match std::env::var_os("AGUI_SPEC_DIR").filter(|d| !d.is_empty()) {
        Some(dir) => PathBuf::from(dir),
        None => workspace().join(".spec-cache").join(&commit),
    };
    if cache.is_dir() {
        let stamp = std::fs::read_to_string(cache.join(".commit")).unwrap_or_default();
        assert_eq!(
            stamp.trim(),
            commit,
            "spec cache at {} is stale or unstamped: run 'cargo xtask spec-fetch'",
            cache.display()
        );
        return Some(cache);
    }
    let msg = format!(
        "spec cache missing for {commit}: run 'cargo xtask spec-fetch' (looked in {})",
        cache.display()
    );
    assert!(
        std::env::var("AGUI_REQUIRE_SPEC").as_deref() != Ok("1"),
        "{msg} [AGUI_REQUIRE_SPEC=1]"
    );
    eprintln!("SKIPPED: {msg}");
    None
}

/// Directory holding `events.proto`, `types.proto` and `patch.proto`.
pub fn proto_sources_dir() -> Option<PathBuf> {
    root().map(|r| r.join("sdks/typescript/packages/proto/src/proto"))
}

/// `spec/1.0/proto-freeze.txt`.
pub fn proto_freeze() -> Option<PathBuf> {
    root().map(|r| r.join("spec/1.0/proto-freeze.txt"))
}

/// The TypeScript protobuf byte corpus.
pub fn proto_bytes_dir() -> Option<PathBuf> {
    root().map(|r| r.join("sdks/typescript/packages/proto/__tests__/__fixtures__/bytes"))
}

/// `spec/1.0/fixtures`, whose valid documents the byte corpus encodes.
pub fn fixtures_dir() -> Option<PathBuf> {
    root().map(|r| r.join("spec/1.0/fixtures"))
}

/// One byte-corpus case: `<Anchor>__<name>.json.bin` paired with
/// `spec/1.0/fixtures/<Anchor>/valid/<name>.json`.
#[derive(Debug, Clone)]
pub struct ByteCase {
    /// `<Anchor>__<name>`.
    pub name: String,
    /// The fixture's JSON text, byte for byte.
    pub json: Vec<u8>,
    /// The canonical encoding.
    pub bin: Vec<u8>,
}

/// Every byte-corpus case, discovered from the directory (the count is
/// printed, not asserted), sorted by name. `None` when the cache is missing.
pub fn byte_cases() -> Option<Vec<ByteCase>> {
    let bytes = proto_bytes_dir()?;
    let fixtures = fixtures_dir()?;
    let mut out = Vec::new();
    for entry in std::fs::read_dir(&bytes).expect("read byte corpus directory") {
        let path = entry.expect("directory entry").path();
        let file = path.file_name().and_then(|f| f.to_str()).unwrap_or("");
        let Some(name) = file.strip_suffix(".json.bin") else {
            continue;
        };
        let (anchor, case) = name
            .split_once("__")
            .unwrap_or_else(|| panic!("{file}: not <Anchor>__<name>.json.bin"));
        let json_path = fixtures
            .join(anchor)
            .join("valid")
            .join(format!("{case}.json"));
        let json = std::fs::read(&json_path)
            .unwrap_or_else(|e| panic!("{}: no paired fixture: {e}", json_path.display()));
        let bin = std::fs::read(&path).expect("read .bin");
        out.push(ByteCase {
            name: name.to_owned(),
            json,
            bin,
        });
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    assert!(!out.is_empty(), "no byte cases in {}", bytes.display());
    eprintln!("protobuf byte corpus: {} cases", out.len());
    Some(out)
}

/// Members carried as `google.protobuf.Value` / `Struct` (events.proto,
/// types.proto, patch.proto): open JSON, not protocol fields. `value` is
/// open only on CUSTOM and on a JSON Patch op (see `enters_open`); elsewhere
/// (`Context`, input-content sources) it is a plain string.
const OPEN: &[&str] = &[
    "snapshot",
    "value",
    "event",
    "rawEvent",
    "metadata",
    "result",
    "parameters",
    "payload",
    "state",
    "forwardedProps",
    "responseSchema",
];

/// Semantic equality between a fixture and a decoded event. The only slack
/// is protobuf's implicit presence (PROC-09): a non-optional string or a
/// repeated field holding its default (`""`, `[]`) is not on the wire, so it
/// may be absent on one side and default on the other. That slack stops at
/// open-JSON members (`snapshot`, `value`, `metadata`, an object `content`,
/// ...): inside a `Value` an empty string, an empty array and a present key
/// are explicit on the wire, so a decoder that drops one must not compare
/// equal. Numbers compare as `f64`, which is what both wires carry.
pub fn sem_eq(a: &serde_json::Value, b: &serde_json::Value) -> bool {
    use serde_json::Value;
    fn is_default(v: &Value, open: bool) -> bool {
        !open
            && (matches!(v, Value::String(s) if s.is_empty())
                || matches!(v, Value::Array(a) if a.is_empty()))
    }
    fn enters_open(parent: &serde_json::Map<String, Value>, key: &str, v: &Value) -> bool {
        if key == "value" {
            return parent.get("type").and_then(Value::as_str) == Some("CUSTOM")
                || parent.contains_key("op");
        }
        OPEN.contains(&key) || (key == "content" && v.is_object())
    }
    fn eq(a: &Value, b: &Value, open: bool) -> bool {
        match (a, b) {
            (Value::Object(x), Value::Object(y)) => {
                x.iter().all(|(k, v)| match y.get(k) {
                    Some(w) => eq(v, w, open || enters_open(x, k, v)),
                    None => is_default(v, open || enters_open(x, k, v)),
                }) && y
                    .iter()
                    .all(|(k, w)| x.contains_key(k) || is_default(w, open || enters_open(y, k, w)))
            }
            (Value::Array(x), Value::Array(y)) => {
                x.len() == y.len() && x.iter().zip(y).all(|(v, w)| eq(v, w, open))
            }
            (Value::Number(x), Value::Number(y)) => x.as_f64() == y.as_f64(),
            _ => a == b,
        }
    }
    eq(a, b, false)
}

/// Hand-built protobuf: a varint.
pub fn varint(mut v: u64, out: &mut Vec<u8>) {
    while v >= 0x80 {
        out.push((v as u8) | 0x80);
        v >>= 7;
    }
    out.push(v as u8);
}

/// Hand-built protobuf: a length-delimited field.
pub fn ld(field: u32, body: &[u8]) -> Vec<u8> {
    let mut v = Vec::new();
    varint((u64::from(field) << 3) | 2, &mut v);
    varint(body.len() as u64, &mut v);
    v.extend_from_slice(body);
    v
}

/// Hand-built protobuf: a varint field.
pub fn vi(field: u32, x: u64) -> Vec<u8> {
    let mut v = Vec::new();
    varint(u64::from(field) << 3, &mut v);
    varint(x, &mut v);
    v
}
