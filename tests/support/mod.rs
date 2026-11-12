//! Spec-data access for the protocol-type tests.
//!
//! The same lookup rule as `agui_test_support::spec`/`corpus`, kept local
//! so these tests do not depend on the harness crate.
//!
//! Location: `$AGUI_SPEC_DIR` (the root of an upstream checkout or extracted
//! archive) if set, else `<workspace>/.spec-cache/<commit>` with `commit`
//! read from `spec.lock`. A missing tree prints a skip notice, or fails when
//! `AGUI_REQUIRE_SPEC=1`.

#![allow(dead_code)]

use std::fs;
use std::path::{Path, PathBuf};

/// One document of the upstream value corpus (`spec/1.0/fixtures`).
pub struct ValueDocument {
    /// The `$defs` anchor (the fixture's top-level directory).
    pub anchor: String,
    /// Path relative to `spec/1.0/fixtures`.
    pub relative: String,
    /// Raw bytes.
    pub bytes: Vec<u8>,
}

fn workspace() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn pinned_commit() -> String {
    let lock = fs::read_to_string(workspace().join("spec.lock")).expect("read spec.lock");
    lock.lines()
        .find_map(|line| {
            let rest = line.trim().strip_prefix("commit")?.trim_start();
            let value = rest.strip_prefix('=')?.trim();
            Some(value.trim_matches('"').to_owned())
        })
        .expect("spec.lock names a commit")
}

/// Root of the upstream tree, or `None` (after printing why) when absent.
/// A `.commit` stamp, wherever the tree is, must name the pinned commit.
pub fn root() -> Option<PathBuf> {
    let commit = pinned_commit();
    // An empty `$AGUI_SPEC_DIR` means unset, as in `tests/proto_support`.
    let cache = std::env::var_os("AGUI_SPEC_DIR")
        .filter(|d| !d.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| workspace().join(".spec-cache").join(&commit));
    if cache.is_dir() {
        if let Ok(stamp) = fs::read_to_string(cache.join(".commit")) {
            assert_eq!(
                stamp.trim(),
                commit,
                "stale spec cache: run 'cargo xtask spec-fetch'"
            );
        }
        return Some(cache);
    }
    let message = format!("spec cache missing for {commit}: run 'cargo xtask spec-fetch'");
    assert!(
        std::env::var("AGUI_REQUIRE_SPEC").as_deref() != Ok("1"),
        "{message}"
    );
    eprintln!("{message}");
    None
}

fn fixtures_dir(root: &Path) -> PathBuf {
    root.join("spec/1.0/fixtures")
}

/// Every document of the value corpus, `valid` or `invalid`, discovered from
/// the upstream `MANIFEST.txt` and checked against the tree.
pub fn value_documents(valid: bool) -> Option<Vec<ValueDocument>> {
    let root = root()?;
    let dir = fixtures_dir(&root);
    let manifest =
        fs::read_to_string(dir.join("MANIFEST.txt")).expect("read fixtures MANIFEST.txt");
    let wanted = if valid { "/valid/" } else { "/invalid/" };
    let mut documents = Vec::new();
    for line in manifest.lines() {
        let line = line.trim();
        if line.is_empty()
            || line.starts_with('#')
            || line.ends_with(".expect.json")
            || !line.contains(wanted)
        {
            continue;
        }
        let anchor = line.split('/').next().expect("anchor").to_owned();
        let bytes = fs::read(dir.join(line)).unwrap_or_else(|e| panic!("{line}: {e}"));
        documents.push(ValueDocument {
            anchor,
            relative: line.to_owned(),
            bytes,
        });
    }
    // The manifest must not under-list the tree.
    let mut on_disk = 0;
    for anchor in fs::read_dir(&dir).expect("list fixtures") {
        let sub = anchor.expect("entry").path().join(wanted.trim_matches('/'));
        if let Ok(entries) = fs::read_dir(sub) {
            on_disk += entries
                .filter_map(Result::ok)
                .filter(|e| {
                    let name = e.file_name();
                    let name = name.to_string_lossy();
                    name.ends_with(".json") && !name.ends_with(".expect.json")
                })
                .count();
        }
    }
    assert_eq!(
        on_disk,
        documents.len(),
        "MANIFEST.txt disagrees with the fixture tree"
    );
    eprintln!(
        "value corpus: {} {} documents",
        documents.len(),
        wanted.trim_matches('/')
    );
    Some(documents)
}
