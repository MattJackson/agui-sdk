//! The spec cache.
//!
//! `cargo xtask spec-fetch` extracts the paths `spec.lock` lists from the
//! pinned upstream archive into `<workspace>/.spec-cache/<commit>/` and writes
//! the commit it extracted into `<cache>/.commit`. Tests locate that tree here.
//! `$AGUI_SPEC_DIR` points at a different root with the same layout (and the
//! same `.commit` stamp), for example a cache shared between worktrees.

use std::path::{Path, PathBuf};

use serde::Deserialize;

/// Environment variable overriding the cache location.
pub const SPEC_DIR_ENV: &str = "AGUI_SPEC_DIR";

/// Environment variable that turns a missing cache from a skip into a
/// failure. CI sets it, so the conformance gate cannot pass without the data.
pub const REQUIRE_SPEC_ENV: &str = "AGUI_REQUIRE_SPEC";

/// Name of the stamp file inside a cache root.
pub const STAMP_FILE: &str = ".commit";

/// `spec.lock`, parsed.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[non_exhaustive]
pub struct SpecLock {
    /// Upstream repository URL.
    pub upstream: String,
    /// Pinned commit.
    pub commit: String,
    /// Archive digest (may be empty until the first `spec-bump`).
    #[serde(default)]
    pub sha256: String,
    /// Paths extracted from the archive.
    pub paths: Vec<String>,
}

impl SpecLock {
    /// Parses `spec.lock` text.
    ///
    /// # Errors
    ///
    /// The TOML error, or a commit that is not 40 lowercase hex digits.
    pub fn parse(text: &str) -> Result<Self, String> {
        let lock: SpecLock = toml::from_str(text).map_err(|e| format!("spec.lock: {e}"))?;
        let hex = lock.commit.len() == 40
            && lock
                .commit
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
        if !hex {
            return Err(format!(
                "spec.lock: commit {:?} is not a full lowercase sha",
                lock.commit
            ));
        }
        Ok(lock)
    }

    /// Reads `<workspace>/spec.lock`.
    ///
    /// # Panics
    ///
    /// When the file is missing or invalid: every corpus test depends on it.
    pub fn load() -> Self {
        let path = crate::workspace_root().join("spec.lock");
        let text = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()));
        Self::parse(&text).unwrap_or_else(|e| panic!("{e}"))
    }
}

/// The pinned upstream commit, read from `spec.lock`.
pub fn pinned_commit() -> String {
    SpecLock::load().commit
}

/// Where the cache stands, without side effects.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum CacheStatus {
    /// The cache exists and its stamp matches the pin.
    Ready(PathBuf),
    /// No cache at `path`.
    Missing {
        /// The pinned commit.
        commit: String,
        /// Where it was looked for.
        path: PathBuf,
    },
    /// A cache exists but was fetched for another commit (or has no stamp).
    Stale {
        /// The pinned commit.
        expected: String,
        /// What the stamp says (empty when there is none).
        found: String,
        /// The cache root.
        path: PathBuf,
    },
}

/// Locates the cache for `commit`: `env_dir` when given, else
/// `<workspace>/.spec-cache/<commit>`.
pub fn locate_in(workspace: &Path, env_dir: Option<&Path>, commit: &str) -> CacheStatus {
    let path = match env_dir {
        Some(dir) => dir.to_path_buf(),
        None => workspace.join(".spec-cache").join(commit),
    };
    if !path.is_dir() {
        return CacheStatus::Missing {
            commit: commit.to_owned(),
            path,
        };
    }
    let found = std::fs::read_to_string(path.join(STAMP_FILE))
        .map(|s| s.trim().to_owned())
        .unwrap_or_default();
    if found == commit {
        CacheStatus::Ready(path)
    } else {
        CacheStatus::Stale {
            expected: commit.to_owned(),
            found,
            path,
        }
    }
}

/// [`locate_in`] with the real workspace, `$AGUI_SPEC_DIR` and `spec.lock`.
pub fn locate() -> CacheStatus {
    let env_dir = std::env::var_os(SPEC_DIR_ENV)
        .filter(|v| !v.is_empty())
        .map(PathBuf::from);
    locate_in(
        &crate::workspace_root(),
        env_dir.as_deref(),
        &pinned_commit(),
    )
}

/// Whether `AGUI_REQUIRE_SPEC=1` is set.
pub fn spec_required() -> bool {
    std::env::var(REQUIRE_SPEC_ENV).as_deref() == Ok("1")
}

/// Root of the extracted upstream tree for the pinned commit:
/// `$AGUI_SPEC_DIR`, else `<workspace>/.spec-cache/<commit>`.
///
/// Returns `None` after printing `spec cache missing for <commit>: run
/// 'cargo xtask spec-fetch'` when the cache is absent; the calling test then
/// returns early (a visible skip).
///
/// # Panics
///
/// When the cache is absent and `AGUI_REQUIRE_SPEC=1`, or when the cache
/// exists but was fetched for a different commit than `spec.lock` pins.
pub fn root() -> Option<PathBuf> {
    match locate() {
        CacheStatus::Ready(path) => Some(path),
        CacheStatus::Missing { commit, path } => {
            let msg = format!(
                "spec cache missing for {commit}: run 'cargo xtask spec-fetch' (looked in {})",
                path.display()
            );
            if spec_required() {
                panic!("{msg} [{REQUIRE_SPEC_ENV}=1]");
            }
            eprintln!("SKIPPED: {msg}");
            None
        }
        CacheStatus::Stale {
            expected,
            found,
            path,
        } => panic!(
            "spec cache at {} is for {:?}, but spec.lock pins {expected}: run 'cargo xtask spec-fetch'",
            path.display(),
            found
        ),
    }
}

/// `spec/1.0/schema.json` in the cache.
pub fn schema_path() -> Option<PathBuf> {
    root().map(|r| r.join("spec/1.0/schema.json"))
}

/// `spec/1.0/conformance/streams` in the cache.
pub fn streams_dir() -> Option<PathBuf> {
    root().map(|r| streams_dir_in(&r))
}

/// `spec/1.0/fixtures` in the cache.
pub fn values_dir() -> Option<PathBuf> {
    root().map(|r| values_dir_in(&r))
}

/// The TypeScript protobuf byte corpus in the cache.
pub fn proto_bytes_dir() -> Option<PathBuf> {
    root().map(|r| proto_bytes_dir_in(&r))
}

/// Stream corpus directory under a cache root.
pub fn streams_dir_in(root: &Path) -> PathBuf {
    root.join("spec/1.0/conformance/streams")
}

/// Value corpus directory under a cache root.
pub fn values_dir_in(root: &Path) -> PathBuf {
    root.join("spec/1.0/fixtures")
}

/// Protobuf byte corpus directory under a cache root.
pub fn proto_bytes_dir_in(root: &Path) -> PathBuf {
    root.join("sdks/typescript/packages/proto/__tests__/__fixtures__/bytes")
}

#[cfg(test)]
mod tests {
    use super::*;

    const SHA: &str = "deefd3833c15b353348ba90eae0fad6113021759";

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("agui-ts-spec-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap_or_else(|e| panic!("{e}"));
        dir
    }

    #[test]
    fn lock_parses_and_rejects_short_commit() {
        let ok = format!("upstream = \"u\"\ncommit = \"{SHA}\"\nsha256 = \"\"\npaths = [\"a\"]\n");
        assert_eq!(SpecLock::parse(&ok).map(|l| l.commit), Ok(SHA.to_owned()));
        let bad = "upstream = \"u\"\ncommit = \"deadbeef\"\npaths = []\n";
        assert!(SpecLock::parse(bad).is_err());
    }

    #[test]
    fn the_real_lock_parses() {
        let lock = SpecLock::load();
        assert!(lock
            .paths
            .iter()
            .any(|p| p == "spec/1.0/conformance/streams"));
    }

    #[test]
    fn missing_ready_and_stale() {
        let ws = scratch("ws");
        assert!(matches!(
            locate_in(&ws, None, SHA),
            CacheStatus::Missing { .. }
        ));
        let root = ws.join(".spec-cache").join(SHA);
        std::fs::create_dir_all(&root).unwrap_or_else(|e| panic!("{e}"));
        // No stamp: stale, never silently accepted.
        assert!(matches!(
            locate_in(&ws, None, SHA),
            CacheStatus::Stale { .. }
        ));
        std::fs::write(
            root.join(STAMP_FILE),
            "0000000000000000000000000000000000000000\n",
        )
        .unwrap_or_else(|e| panic!("{e}"));
        assert!(matches!(
            locate_in(&ws, None, SHA),
            CacheStatus::Stale { .. }
        ));
        std::fs::write(root.join(STAMP_FILE), format!("{SHA}\n")).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(locate_in(&ws, None, SHA), CacheStatus::Ready(root.clone()));
        // An explicit directory wins over the workspace cache.
        let other = scratch("env");
        assert!(matches!(
            locate_in(&ws, Some(&other), SHA),
            CacheStatus::Stale { .. }
        ));
        let _ = std::fs::remove_dir_all(&ws);
        let _ = std::fs::remove_dir_all(&other);
    }
}
