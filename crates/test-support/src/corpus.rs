//! Corpus discovery. Each corpus is enumerated from its directory, checked
//! against its upstream `MANIFEST.txt` in both directions, and its size is
//! printed. An empty corpus in a present cache is a failure: a lane over zero
//! items proves nothing.
//!
//! The `*_in` functions take an explicit directory and return every problem;
//! the convenience functions use the spec cache, print the counts, skip (with
//! the cache's message) when it is absent, and panic on any problem.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::spec;

/// Top-level keys a stream fixture may carry (corpus README, "Adding a
/// fixture"). Anything else fails discovery: a key no lane reads is an
/// assertion nobody checks.
pub const FIXTURE_KEYS: &[&str] = &[
    "name",
    "area",
    "description",
    "specPage",
    "kill",
    "client",
    "input",
    "stream",
    "expect",
    "expectOverrides",
];

/// One stream fixture (`spec/1.0/conformance/streams/*.json`).
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct StreamFixture {
    /// Fixture name (file stem, equal to the document's `name`).
    pub name: String,
    /// Path.
    pub path: PathBuf,
    /// Parsed document.
    pub document: Value,
}

impl StreamFixture {
    /// Parses a fixture from its file.
    ///
    /// # Errors
    ///
    /// Unreadable or non-JSON file, a `name` that differs from the file
    /// stem, a missing `stream` array or `expect` object, or an unknown
    /// top-level key.
    pub fn from_path(path: &Path) -> Result<Self, String> {
        let stem = path
            .file_stem()
            .and_then(|s| s.to_str())
            .ok_or_else(|| format!("{}: no file name", path.display()))?
            .to_owned();
        let bytes = std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
        let document: Value =
            serde_json::from_slice(&bytes).map_err(|e| format!("{stem}: not JSON: {e}"))?;
        let fixture = StreamFixture {
            name: stem,
            path: path.to_path_buf(),
            document,
        };
        fixture.check()?;
        Ok(fixture)
    }

    fn check(&self) -> Result<(), String> {
        let obj = self
            .document
            .as_object()
            .ok_or_else(|| format!("{}: not a JSON object", self.name))?;
        for key in obj.keys() {
            if !FIXTURE_KEYS.contains(&key.as_str()) {
                return Err(format!("{}: unknown top-level key {key:?}", self.name));
            }
        }
        if obj.get("name").and_then(Value::as_str) != Some(self.name.as_str()) {
            return Err(format!("{}: `name` differs from the file name", self.name));
        }
        if !obj.get("stream").is_some_and(Value::is_array) {
            return Err(format!("{}: `stream` is not an array", self.name));
        }
        if !obj.get("expect").is_some_and(Value::is_object) {
            return Err(format!("{}: `expect` is not an object", self.name));
        }
        Ok(())
    }

    /// The raw events, sent verbatim.
    pub fn stream(&self) -> &[Value] {
        self.document
            .get("stream")
            .and_then(Value::as_array)
            .map_or(&[], Vec::as_slice)
    }

    /// The base `expect` block.
    pub fn expect(&self) -> &serde_json::Map<String, Value> {
        static EMPTY: std::sync::OnceLock<serde_json::Map<String, Value>> =
            std::sync::OnceLock::new();
        self.document
            .get("expect")
            .and_then(Value::as_object)
            .unwrap_or_else(|| EMPTY.get_or_init(serde_json::Map::new))
    }

    /// `client.maxProtocolVersion`: the peer era the client is pinned to.
    pub fn max_protocol_version(&self) -> Option<&str> {
        self.document
            .pointer("/client/maxProtocolVersion")
            .and_then(Value::as_str)
    }

    /// `input.messages`: the history the run starts from.
    pub fn input_messages(&self) -> Option<&Vec<Value>> {
        self.document
            .pointer("/input/messages")
            .and_then(Value::as_array)
    }

    /// `input.tools`.
    pub fn input_tools(&self) -> Option<&Vec<Value>> {
        self.document
            .pointer("/input/tools")
            .and_then(Value::as_array)
    }

    /// The fixture's `area`, for grouping output.
    pub fn area(&self) -> &str {
        self.document
            .get("area")
            .and_then(Value::as_str)
            .unwrap_or("")
    }
}

/// One value document (`spec/1.0/fixtures/<Anchor>/<valid|invalid>/*.json`).
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct ValueDocument {
    /// The `$defs` anchor.
    pub anchor: String,
    /// Whether the strict schema accepts it.
    pub valid: bool,
    /// Path relative to the corpus root.
    pub relative: String,
    /// Raw bytes.
    pub bytes: Vec<u8>,
    /// For invalid documents, the sibling `<name>.expect.json` (the schema
    /// keyword and locations that reject it), when present.
    pub expect: Option<Value>,
}

/// One protobuf byte case (`<Anchor>__<name>.json.bin` and its JSON source,
/// `spec/1.0/fixtures/<Anchor>/valid/<name>.json`).
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct ProtoCase {
    /// Case name (`<Anchor>__<name>`).
    pub name: String,
    /// The `$defs` anchor.
    pub anchor: String,
    /// The JSON source document's bytes.
    pub json: Vec<u8>,
    /// The canonical encoding.
    pub bytes: Vec<u8>,
}

/// Reads a `MANIFEST.txt`: one relative path per line; `#` comments and
/// blank lines ignored.
///
/// # Errors
///
/// Unreadable file, or a duplicate entry.
pub fn read_manifest(path: &Path) -> Result<BTreeSet<String>, String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let mut entries = BTreeSet::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if !entries.insert(line.to_owned()) {
            return Err(format!("{}: duplicate entry {line}", path.display()));
        }
    }
    Ok(entries)
}

/// Compares a manifest with what is on disk, in both directions.
///
/// # Errors
///
/// One line per file listed but absent, and per file present but unlisted.
pub fn check_listing(
    corpus: &str,
    manifest: &BTreeSet<String>,
    found: &BTreeSet<String>,
) -> Result<(), Vec<String>> {
    let mut problems = Vec::new();
    for missing in manifest.difference(found) {
        problems.push(format!(
            "{corpus}: {missing} is in MANIFEST.txt but not on disk"
        ));
    }
    for extra in found.difference(manifest) {
        problems.push(format!(
            "{corpus}: {extra} is on disk but not in MANIFEST.txt"
        ));
    }
    if problems.is_empty() {
        Ok(())
    } else {
        Err(problems)
    }
}

/// Every file under `dir` matching `keep`, as `/`-separated relative paths.
fn walk(dir: &Path, keep: &dyn Fn(&str) -> bool) -> Result<BTreeSet<String>, String> {
    let mut out = BTreeSet::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(next) = stack.pop() {
        let entries = std::fs::read_dir(&next).map_err(|e| format!("{}: {e}", next.display()))?;
        for entry in entries {
            let path = entry
                .map_err(|e| format!("{}: {e}", next.display()))?
                .path();
            if path.is_dir() {
                stack.push(path);
                continue;
            }
            let rel = path
                .strip_prefix(dir)
                .map_err(|e| format!("{}: {e}", path.display()))?
                .components()
                .map(|c| c.as_os_str().to_string_lossy().into_owned())
                .collect::<Vec<_>>()
                .join("/");
            if keep(&rel) {
                out.insert(rel);
            }
        }
    }
    Ok(out)
}

fn collect<T>(
    results: Vec<Result<T, String>>,
    mut problems: Vec<String>,
) -> Result<Vec<T>, Vec<String>> {
    let mut ok = Vec::with_capacity(results.len());
    for r in results {
        match r {
            Ok(v) => ok.push(v),
            Err(e) => problems.push(e),
        }
    }
    if problems.is_empty() {
        Ok(ok)
    } else {
        Err(problems)
    }
}

/// Loads the stream corpus from `dir`.
///
/// # Errors
///
/// Manifest mismatches and malformed fixtures, all of them.
pub fn streams_in(dir: &Path) -> Result<Vec<StreamFixture>, Vec<String>> {
    let manifest = read_manifest(&dir.join("MANIFEST.txt")).map_err(|e| vec![e])?;
    let found =
        walk(dir, &|rel| !rel.contains('/') && rel.ends_with(".json")).map_err(|e| vec![e])?;
    let problems = check_listing("streams", &manifest, &found)
        .err()
        .unwrap_or_default();
    let results = found
        .iter()
        .map(|rel| StreamFixture::from_path(&dir.join(rel)))
        .collect();
    collect(results, problems)
}

/// Loads the value corpus from `dir`.
///
/// # Errors
///
/// Manifest mismatches, unexpected layout, and unreadable files.
pub fn values_in(dir: &Path) -> Result<Vec<ValueDocument>, Vec<String>> {
    let manifest = read_manifest(&dir.join("MANIFEST.txt")).map_err(|e| vec![e])?;
    let found = walk(dir, &|rel| rel.ends_with(".json")).map_err(|e| vec![e])?;
    let mut problems = check_listing("fixtures", &manifest, &found)
        .err()
        .unwrap_or_default();
    let mut results = Vec::new();
    for rel in &found {
        if rel.ends_with(".expect.json") {
            let doc = rel.replace(".expect.json", ".json");
            if !found.contains(&doc) {
                problems.push(format!("fixtures: {rel} has no document {doc}"));
            }
            continue;
        }
        let parts: Vec<&str> = rel.split('/').collect();
        let (anchor, valid) = match parts.as_slice() {
            [anchor, "valid", _] => (*anchor, true),
            [anchor, "invalid", _] => (*anchor, false),
            _ => {
                problems.push(format!(
                    "fixtures: {rel} is not <Anchor>/<valid|invalid>/<name>.json"
                ));
                continue;
            }
        };
        let path = dir.join(rel);
        results.push(
            std::fs::read(&path)
                .map_err(|e| format!("{rel}: {e}"))
                .and_then(|bytes| {
                    let expect_rel = rel.replace(".json", ".expect.json");
                    let expect = if found.contains(&expect_rel) {
                        let text = std::fs::read(dir.join(&expect_rel))
                            .map_err(|e| format!("{expect_rel}: {e}"))?;
                        Some(
                            serde_json::from_slice(&text)
                                .map_err(|e| format!("{expect_rel}: not JSON: {e}"))?,
                        )
                    } else {
                        None
                    };
                    Ok(ValueDocument {
                        anchor: anchor.to_owned(),
                        valid,
                        relative: rel.clone(),
                        bytes,
                        expect,
                    })
                }),
        );
    }
    collect(results, problems)
}

/// Loads the protobuf byte corpus from `bytes_dir`, pairing each case with
/// its JSON source under `values_dir`. The byte corpus has no manifest of its
/// own; instead every `.bin` must name a valid value document, and nothing
/// but `.bin` files may sit in the directory.
///
/// # Errors
///
/// Unpaired or unexpected files, and unreadable ones.
pub fn proto_cases_in(bytes_dir: &Path, values_dir: &Path) -> Result<Vec<ProtoCase>, Vec<String>> {
    let found = walk(bytes_dir, &|_| true).map_err(|e| vec![e])?;
    let mut problems = Vec::new();
    let mut results = Vec::new();
    for rel in &found {
        let Some(stem) = rel.strip_suffix(".json.bin") else {
            problems.push(format!("proto bytes: unexpected file {rel}"));
            continue;
        };
        let Some((anchor, name)) = stem.split_once("__") else {
            problems.push(format!(
                "proto bytes: {rel} is not <Anchor>__<name>.json.bin"
            ));
            continue;
        };
        let json_path = values_dir
            .join(anchor)
            .join("valid")
            .join(format!("{name}.json"));
        results.push(
            std::fs::read(&json_path)
                .map_err(|e| format!("proto bytes: {rel}: source {}: {e}", json_path.display()))
                .and_then(|json| {
                    let bytes = std::fs::read(bytes_dir.join(rel))
                        .map_err(|e| format!("proto bytes: {rel}: {e}"))?;
                    Ok(ProtoCase {
                        name: stem.to_owned(),
                        anchor: anchor.to_owned(),
                        json,
                        bytes,
                    })
                }),
        );
    }
    collect(results, problems)
}

fn or_panic<T>(corpus: &str, result: Result<Vec<T>, Vec<String>>) -> Vec<T> {
    match result {
        Ok(items) => items,
        Err(problems) => panic!(
            "{corpus} corpus is inconsistent:\n  {}",
            problems.join("\n  ")
        ),
    }
}

/// The stream corpus, or `None` (a printed skip) when the cache is absent.
///
/// # Panics
///
/// On any manifest mismatch or malformed fixture.
pub fn streams() -> Option<Vec<StreamFixture>> {
    let dir = spec::streams_dir()?;
    let items = non_empty("stream", or_panic("stream", streams_in(&dir)));
    eprintln!("stream corpus: {} fixtures", items.len());
    Some(items)
}

/// The value corpus, or `None` (a printed skip) when the cache is absent.
///
/// # Panics
///
/// On any manifest mismatch or unreadable document.
pub fn values() -> Option<Vec<ValueDocument>> {
    let dir = spec::values_dir()?;
    let items = non_empty("value", or_panic("value", values_in(&dir)));
    let valid = items.iter().filter(|d| d.valid).count();
    eprintln!(
        "value corpus: {} documents ({valid} valid, {} invalid)",
        items.len(),
        items.len() - valid
    );
    Some(items)
}

/// The protobuf byte corpus, or `None` (a printed skip) when the cache is
/// absent.
///
/// # Panics
///
/// On any unpaired or unexpected file.
pub fn proto_cases() -> Option<Vec<ProtoCase>> {
    let root = spec::root()?;
    let items = non_empty(
        "protobuf byte",
        or_panic(
            "protobuf byte",
            proto_cases_in(
                &spec::proto_bytes_dir_in(&root),
                &spec::values_dir_in(&root),
            ),
        ),
    );
    eprintln!("protobuf byte corpus: {} cases", items.len());
    Some(items)
}

/// Panics when a corpus the cache provides has no items.
fn non_empty<T>(what: &str, items: Vec<T>) -> Vec<T> {
    assert!(
        !items.is_empty(),
        "the {what} corpus in the spec cache is empty: a lane over nothing proves nothing"
    );
    items
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("agui-ts-corpus-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap_or_else(|e| panic!("{e}"));
        dir
    }

    fn write(path: &Path, text: &str) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap_or_else(|e| panic!("{e}"));
        }
        std::fs::write(path, text).unwrap_or_else(|e| panic!("{e}"));
    }

    const FIXTURE: &str = r#"{"name":"a","area":"x","description":"d","specPage":"/p","kill":"k","stream":[],"expect":{"outcome":"completed"}}"#;

    #[test]
    fn manifest_mismatch_is_reported_both_ways() {
        let dir = scratch("streams");
        write(&dir.join("MANIFEST.txt"), "# header\n\na.json\ngone.json\n");
        write(&dir.join("a.json"), FIXTURE);
        write(&dir.join("b.json"), &FIXTURE.replace("\"a\"", "\"b\""));
        let problems = streams_in(&dir).err().unwrap_or_default();
        assert_eq!(problems.len(), 2, "{problems:?}");
        assert!(problems[0].contains("gone.json is in MANIFEST.txt"));
        assert!(problems[1].contains("b.json is on disk"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn unknown_top_level_key_and_name_mismatch_fail() {
        let dir = scratch("keys");
        write(&dir.join("MANIFEST.txt"), "a.json\nc.json\n");
        write(
            &dir.join("a.json"),
            &FIXTURE.replace("\"kill\"", "\"future\""),
        );
        write(&dir.join("c.json"), FIXTURE);
        let problems = streams_in(&dir).err().unwrap_or_default();
        assert!(
            problems
                .iter()
                .any(|p| p.contains("unknown top-level key \"future\"")),
            "{problems:?}"
        );
        assert!(
            problems.iter().any(|p| p.contains("c: `name` differs")),
            "{problems:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn values_pair_expect_files_and_proto_cases_pair_sources() {
        let dir = scratch("values");
        let values = dir.join("fixtures");
        write(
            &values.join("MANIFEST.txt"),
            "X/invalid/bad.expect.json\nX/invalid/bad.json\nX/valid/ok.json\n",
        );
        write(&values.join("X/valid/ok.json"), "{}");
        write(&values.join("X/invalid/bad.json"), "[]");
        write(
            &values.join("X/invalid/bad.expect.json"),
            r#"{"keyword":"type"}"#,
        );
        let docs = values_in(&values).unwrap_or_else(|p| panic!("{p:?}"));
        assert_eq!(docs.len(), 2);
        assert!(docs.iter().any(|d| !d.valid && d.expect.is_some()));

        let bytes = dir.join("bytes");
        write(&bytes.join("X__ok.json.bin"), "\u{1}");
        let cases = proto_cases_in(&bytes, &values).unwrap_or_else(|p| panic!("{p:?}"));
        assert_eq!(cases[0].name, "X__ok");
        write(&bytes.join("X__nosource.json.bin"), "\u{1}");
        write(&bytes.join("README"), "");
        let problems = proto_cases_in(&bytes, &values).err().unwrap_or_default();
        assert_eq!(problems.len(), 2, "{problems:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Runs against the real cache when it is present.
    #[test]
    fn real_corpora_load_when_cached() {
        let Some(streams) = streams() else { return };
        assert!(!streams.is_empty());
        let values = values().unwrap_or_default();
        assert!(values.iter().any(|d| d.valid) && values.iter().any(|d| !d.valid));
        let cases = proto_cases().unwrap_or_default();
        assert!(!cases.is_empty());
    }
}
