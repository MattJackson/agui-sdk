//! `spec.lock`: which upstream data the tests read, and its hashes.
//!
//! A deliberately small TOML subset, read and rewritten by hand so the xtask
//! needs no dependencies: `key = "string"` lines, one `paths = [ ... ]`
//! array of strings, and `#` comments, which a rewrite preserves.

use std::path::{Path, PathBuf};

/// The parsed lock.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Lock {
    /// The upstream repository (`https://github.com/<owner>/<repo>`).
    pub upstream: String,
    /// The pinned commit (40 lower-case hex digits).
    pub commit: String,
    /// SHA-256 of the GitHub archive of `commit`.
    pub sha256: String,
    /// SHA-256 over the extracted `paths` (see [`crate::fetch::tree_digest`]).
    pub tree_sha256: String,
    /// Where the schema is published.
    pub schema_url: String,
    /// SHA-256 of the published schema, which equals `spec/1.0/schema.json`
    /// at `commit` byte for byte.
    pub schema_sha256: String,
    /// Files and directories, relative to the repository root, extracted
    /// into the cache.
    pub paths: Vec<String>,
}

/// The file name, at the workspace root.
pub const FILE: &str = "spec.lock";

/// The workspace root: the nearest ancestor of the current directory (or
/// of this crate's manifest) holding `spec.lock`.
pub fn workspace_root() -> Result<PathBuf, String> {
    let starts = [
        std::env::current_dir().ok(),
        Some(PathBuf::from(env!("CARGO_MANIFEST_DIR"))),
    ];
    for start in starts.into_iter().flatten() {
        if let Some(dir) = start.ancestors().find(|d| d.join(FILE).is_file()) {
            return Ok(dir.to_path_buf());
        }
    }
    Err(format!("no {FILE} found above the current directory"))
}

impl Lock {
    /// Reads `<root>/spec.lock`.
    pub fn read(root: &Path) -> Result<Lock, String> {
        let path = root.join(FILE);
        let text = std::fs::read_to_string(&path)
            .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
        Lock::parse(&text)
    }

    /// Parses lock text.
    pub fn parse(text: &str) -> Result<Lock, String> {
        let string = |key: &str| -> Result<String, String> {
            find_string(text, key)?.ok_or_else(|| format!("{FILE}: missing `{key}`"))
        };
        let lock = Lock {
            upstream: string("upstream")?,
            commit: string("commit")?,
            sha256: string("sha256")?,
            tree_sha256: string("tree_sha256")?,
            schema_url: string("schema_url")?,
            schema_sha256: string("schema_sha256")?,
            paths: parse_paths(text)?,
        };
        if !is_commit(&lock.commit) {
            return Err(format!("{FILE}: `commit` must be 40 lower-case hex digits"));
        }
        for p in &lock.paths {
            if p.is_empty() || p.starts_with('/') || p.split('/').any(|s| s == ".." || s.is_empty())
            {
                return Err(format!(
                    "{FILE}: path {p:?} must be relative and normalised"
                ));
            }
        }
        Ok(lock)
    }

    /// Fails unless every hash is filled in.
    pub fn require_hashes(&self) -> Result<(), String> {
        for (key, value) in [
            ("sha256", &self.sha256),
            ("tree_sha256", &self.tree_sha256),
            ("schema_sha256", &self.schema_sha256),
        ] {
            if !is_sha256(value) {
                return Err(format!(
                    "{FILE}: `{key}` is not a SHA-256; pin with `cargo xtask spec-bump <sha>`"
                ));
            }
        }
        Ok(())
    }

    /// `https://codeload.github.com/<owner>/<repo>/tar.gz/<commit>`.
    pub fn archive_url(&self, commit: &str) -> Result<String, String> {
        let repo = self
            .upstream
            .strip_prefix("https://github.com/")
            .map(|r| r.trim_end_matches('/'))
            .filter(|r| r.split('/').count() == 2)
            .ok_or_else(|| {
                format!("{FILE}: `upstream` must be https://github.com/<owner>/<repo>")
            })?;
        Ok(format!(
            "https://codeload.github.com/{repo}/tar.gz/{commit}"
        ))
    }

    /// The top-level directory inside the archive.
    pub fn archive_prefix(&self, commit: &str) -> String {
        let repo = self
            .upstream
            .trim_end_matches('/')
            .rsplit('/')
            .next()
            .unwrap_or_default();
        format!("{repo}-{commit}")
    }
}

/// Whether `s` is a full commit id.
pub fn is_commit(s: &str) -> bool {
    is_lower_hex(s, 40)
}

fn is_sha256(s: &str) -> bool {
    is_lower_hex(s, 64)
}

fn is_lower_hex(s: &str, len: usize) -> bool {
    s.len() == len && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

fn key_of(line: &str) -> Option<&str> {
    let line = line.trim_start();
    if line.starts_with('#') {
        return None;
    }
    line.split_once('=').map(|(k, _)| k.trim())
}

fn find_string(text: &str, key: &str) -> Result<Option<String>, String> {
    let mut found = None;
    for line in text.lines() {
        if key_of(line) != Some(key) {
            continue;
        }
        if found.is_some() {
            return Err(format!("{FILE}: `{key}` appears twice"));
        }
        let value = line
            .split_once('=')
            .map(|(_, v)| v.trim())
            .unwrap_or_default();
        let inner = value
            .strip_prefix('"')
            .and_then(|v| v.strip_suffix('"'))
            .filter(|v| !v.contains('"') && !v.contains('\\'))
            .ok_or_else(|| format!("{FILE}: `{key}` must be a plain \"string\""))?;
        found = Some(inner.to_owned());
    }
    Ok(found)
}

fn parse_paths(text: &str) -> Result<Vec<String>, String> {
    let start = text
        .lines()
        .position(|l| key_of(l) == Some("paths"))
        .ok_or_else(|| format!("{FILE}: missing `paths`"))?;
    let mut body = String::new();
    for line in text.lines().skip(start) {
        let line = line.split('#').next().unwrap_or_default();
        body.push_str(line);
        if line.contains(']') {
            break;
        }
    }
    let inner = body
        .split_once('[')
        .and_then(|(_, rest)| rest.split_once(']'))
        .map(|(inner, _)| inner)
        .ok_or_else(|| format!("{FILE}: `paths` must be an array"))?;
    let mut paths = Vec::new();
    for item in inner.split(',').map(str::trim).filter(|s| !s.is_empty()) {
        let p = item
            .strip_prefix('"')
            .and_then(|v| v.strip_suffix('"'))
            .ok_or_else(|| format!("{FILE}: bad path entry {item}"))?;
        paths.push(p.to_owned());
    }
    if paths.is_empty() {
        return Err(format!("{FILE}: `paths` is empty"));
    }
    Ok(paths)
}

/// Rewrites the `key = "..."` line's value, keeping everything else.
pub fn set_string(text: &str, key: &str, value: &str) -> Result<String, String> {
    let mut out = String::with_capacity(text.len());
    let mut done = false;
    for line in text.split_inclusive('\n') {
        if key_of(line) == Some(key) {
            let indent = &line[..line.len() - line.trim_start().len()];
            let eol = if line.ends_with('\n') { "\n" } else { "" };
            out.push_str(&format!("{indent}{key} = \"{value}\"{eol}"));
            done = true;
        } else {
            out.push_str(line);
        }
    }
    if done {
        Ok(out)
    } else {
        Err(format!("{FILE}: missing `{key}`"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "# comment\nupstream = \"https://github.com/o/r\"\ncommit = \"0123456789abcdef0123456789abcdef01234567\"\n\
        sha256 = \"\"\ntree_sha256 = \"\"\nschema_url = \"https://x/schema.json\"\nschema_sha256 = \"\"\n\
        paths = [\n  \"a/b.json\", # why\n  \"c\",\n]\n";

    #[test]
    fn parses_and_rewrites() -> Result<(), String> {
        let lock = Lock::parse(SAMPLE)?;
        assert_eq!(lock.paths, ["a/b.json", "c"]);
        assert!(lock.require_hashes().is_err());
        assert_eq!(
            lock.archive_url(&lock.commit)?,
            "https://codeload.github.com/o/r/tar.gz/0123456789abcdef0123456789abcdef01234567"
        );
        assert_eq!(
            lock.archive_prefix(&lock.commit),
            "r-0123456789abcdef0123456789abcdef01234567"
        );
        let rewritten = set_string(SAMPLE, "sha256", &"a".repeat(64))?;
        assert!(rewritten.starts_with("# comment\n"));
        assert_eq!(Lock::parse(&rewritten)?.sha256, "a".repeat(64));
        assert_eq!(Lock::parse(&rewritten)?.tree_sha256, "");
        Ok(())
    }

    #[test]
    fn rejects_bad_locks() {
        assert!(Lock::parse(&SAMPLE.replace("0123456789abcdef0123", "XYZ")).is_err());
        assert!(Lock::parse(&SAMPLE.replace("\"c\"", "\"../c\"")).is_err());
        assert!(Lock::parse(&SAMPLE.replace("commit", "# commit")).is_err());
    }
}
