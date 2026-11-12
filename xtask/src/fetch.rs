//! `spec-fetch` and `spec-bump`: download the pinned upstream data into the
//! gitignored `.spec-cache/<commit>/`, verifying every hash.
//!
//! Downloads shell out to `curl` and extraction to `tar` (both ship with
//! Linux, macOS and Windows 10+); hashing is done in-process.

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::lock::{self, Lock};
use crate::sha256;

/// Overrides the cache location (also honoured by the tests).
pub const SPEC_DIR_ENV: &str = "AGUI_SPEC_DIR";
/// The stamp written into a complete cache, holding the commit.
pub const STAMP: &str = ".commit";

/// Hashes describing one fetched commit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hashes {
    pub archive: String,
    pub tree: String,
    pub schema: String,
}

/// `$AGUI_SPEC_DIR`, else `<root>/.spec-cache/<commit>`.
pub fn cache_dir(root: &Path, commit: &str) -> PathBuf {
    match std::env::var_os(SPEC_DIR_ENV) {
        Some(dir) if !dir.is_empty() => PathBuf::from(dir),
        _ => root.join(".spec-cache").join(commit),
    }
}

/// The cache for the pinned commit, after checking its stamp.
pub fn pinned_cache(root: &Path, lock: &Lock) -> Result<PathBuf, String> {
    let dir = cache_dir(root, &lock.commit);
    match std::fs::read_to_string(dir.join(STAMP)) {
        Ok(stamp) if stamp.trim() == lock.commit => Ok(dir),
        Ok(stamp) => Err(format!(
            "spec cache {} is for {}, but spec.lock pins {}: run 'cargo xtask spec-fetch'",
            dir.display(),
            stamp.trim(),
            lock.commit
        )),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Err(format!(
            "spec cache missing for {}: run 'cargo xtask spec-fetch'",
            lock.commit
        )),
        Err(e) => Err(format!(
            "spec cache {}: cannot read {STAMP}: {e}; run 'cargo xtask spec-fetch'",
            dir.display()
        )),
    }
}

/// SHA-256 over every file under `paths` in `dir`: the hex digest of the
/// lines `<sha256 of file>  <relative path with '/'>\n`, sorted by path.
/// Independent of archive compression, so it is what proves a cache matches
/// the lock.
pub fn tree_digest(dir: &Path, paths: &[String]) -> Result<String, String> {
    let mut files = Vec::new();
    for p in paths {
        let full = dir.join(p);
        if !full.exists() {
            return Err(format!("{} is missing", full.display()));
        }
        collect(&full, p, &mut files)?;
    }
    files.sort();
    files.dedup();
    let mut listing = String::new();
    for (rel, full) in &files {
        let hash = sha256::file_hex(full).map_err(|e| format!("{}: {e}", full.display()))?;
        listing.push_str(&hash);
        listing.push_str("  ");
        listing.push_str(rel);
        listing.push('\n');
    }
    Ok(sha256::digest_hex(listing.as_bytes()))
}

fn collect(full: &Path, rel: &str, out: &mut Vec<(String, PathBuf)>) -> Result<(), String> {
    let meta = std::fs::symlink_metadata(full).map_err(|e| format!("{}: {e}", full.display()))?;
    if meta.is_file() {
        out.push((rel.to_owned(), full.to_path_buf()));
    } else if meta.is_dir() {
        let entries = std::fs::read_dir(full).map_err(|e| format!("{}: {e}", full.display()))?;
        for entry in entries {
            let entry = entry.map_err(|e| e.to_string())?;
            let name = entry
                .file_name()
                .into_string()
                .map_err(|n| format!("non-UTF-8 file name {n:?}"))?;
            collect(&entry.path(), &format!("{rel}/{name}"), out)?;
        }
    } else {
        return Err(format!(
            "{} is neither a file nor a directory",
            full.display()
        ));
    }
    Ok(())
}

fn schema_rel(lock: &Lock) -> Result<&str, String> {
    lock.paths
        .iter()
        .map(String::as_str)
        .find(|p| p.ends_with("schema.json"))
        .ok_or_else(|| "spec.lock `paths` lists no schema.json".to_owned())
}

/// `cargo xtask spec-fetch`.
pub fn spec_fetch(root: &Path) -> Result<(), String> {
    let lock = Lock::read(root)?;
    lock.require_hashes()?;
    let dir = cache_dir(root, &lock.commit);
    if pinned_cache(root, &lock).is_ok() {
        if tree_digest(&dir, &lock.paths).ok().as_deref() == Some(lock.tree_sha256.as_str()) {
            println!("spec cache up to date: {}", dir.display());
            return Ok(());
        }
        eprintln!(
            "spec cache {} does not match spec.lock; fetching again",
            dir.display()
        );
    }
    fetch_into(root, &lock, &lock.commit, Some(&lock), None)?;
    println!("fetched {} into {}", lock.commit, dir.display());
    Ok(())
}

/// Downloads `commit`, checks it against `expected` (when pinned), and
/// installs it as the cache for `commit`. When `published_schema` (the hash
/// of the schema published today) is given, the schema at `commit` must match
/// it, or nothing is installed.
fn fetch_into(
    root: &Path,
    lock: &Lock,
    commit: &str,
    expected: Option<&Lock>,
    published_schema: Option<&str>,
) -> Result<Hashes, String> {
    let dest = cache_dir(root, commit);
    let base = dest
        .parent()
        .map(Path::to_path_buf)
        .ok_or("spec cache directory has no parent directory")?;
    std::fs::create_dir_all(&base).map_err(|e| format!("{}: {e}", base.display()))?;
    sweep_scratch(&base);
    let archive = base.join(format!(".archive-{commit}-{}.tar.gz", std::process::id()));
    let result = download(&lock.archive_url(commit)?, &archive)
        .and_then(|()| install_archive(lock, commit, expected, published_schema, &archive, &dest));
    let _ = std::fs::remove_file(&archive);
    result
}

/// Verifies the downloaded `archive` and, only if every check passes,
/// replaces `dest` with its extraction.
fn install_archive(
    lock: &Lock,
    commit: &str,
    expected: Option<&Lock>,
    published_schema: Option<&str>,
    archive: &Path,
    dest: &Path,
) -> Result<Hashes, String> {
    let base = dest
        .parent()
        .ok_or("spec cache directory has no parent directory")?;
    let staging = base.join(format!(".staging-{commit}-{}", std::process::id()));
    let result = (|| {
        remove(&staging)?;
        let archive_hash = sha256::file_hex(archive).map_err(|e| e.to_string())?;
        if let Some(pin) = expected {
            if archive_hash != pin.sha256 {
                return Err(format!(
                    "archive hash mismatch for {commit}: got {archive_hash}, spec.lock says {}. \
                     If GitHub changed how it builds archives, re-pin with `cargo xtask spec-bump {commit}` \
                     and review the tree hash, which must not change.",
                    pin.sha256
                ));
            }
        }
        std::fs::create_dir_all(&staging).map_err(|e| e.to_string())?;
        let prefix = lock.archive_prefix(commit);
        let mut tar = Command::new("tar");
        tar.arg("-xzf")
            .arg(archive)
            .arg("-C")
            .arg(&staging)
            .arg("--strip-components=1");
        for p in &lock.paths {
            tar.arg(format!("{prefix}/{p}"));
        }
        run(&mut tar, "tar")?;
        let hashes = Hashes {
            archive: archive_hash,
            tree: tree_digest(&staging, &lock.paths)?,
            schema: sha256::file_hex(&staging.join(schema_rel(lock)?))
                .map_err(|e| e.to_string())?,
        };
        if let Some(pin) = expected {
            if hashes.tree != pin.tree_sha256 {
                return Err(format!(
                    "extracted files do not match spec.lock tree_sha256 ({} vs {})",
                    hashes.tree, pin.tree_sha256
                ));
            }
            if hashes.schema != pin.schema_sha256 {
                return Err(format!(
                    "schema.json does not match spec.lock schema_sha256 ({} vs {})",
                    hashes.schema, pin.schema_sha256
                ));
            }
        }
        if let Some(published) = published_schema {
            if published != hashes.schema {
                return Err(format!(
                    "the schema published at {} (sha256 {published}) is not byte-identical to \
                     {} at {commit} (sha256 {}); refusing to pin",
                    lock.schema_url,
                    schema_rel(lock)?,
                    hashes.schema
                ));
            }
        }
        std::fs::write(staging.join(STAMP), format!("{commit}\n")).map_err(|e| e.to_string())?;
        replace_dir(&staging, dest)?;
        Ok(hashes)
    })();
    let _ = remove(&staging);
    result
}

/// `cargo xtask spec-bump <sha>`: pins `sha`, after checking that the
/// published schema is byte-identical to the schema at `sha`.
pub fn spec_bump(root: &Path, commit: &str) -> Result<(), String> {
    if !lock::is_commit(commit) {
        return Err("spec-bump needs a full 40-digit lower-case commit id".to_owned());
    }
    let lock = Lock::read(root)?;
    let published_hash = published_schema_hash(root, &lock)?;
    let hashes = fetch_into(root, &lock, commit, None, Some(&published_hash))?;
    let dir = cache_dir(root, commit);

    let path = root.join(lock::FILE);
    let mut text = std::fs::read_to_string(&path).map_err(|e| e.to_string())?;
    text = lock::set_string(&text, "commit", commit)?;
    text = lock::set_string(&text, "sha256", &hashes.archive)?;
    text = lock::set_string(&text, "tree_sha256", &hashes.tree)?;
    text = lock::set_string(&text, "schema_sha256", &hashes.schema)?;
    Lock::parse(&text)?.require_hashes()?;
    std::fs::write(&path, text).map_err(|e| e.to_string())?;
    println!(
        "pinned {commit}\n  archive sha256 {}\n  tree sha256    {}\n  schema sha256  {} (matches {})\n  cache          {}",
        hashes.archive,
        hashes.tree,
        hashes.schema,
        lock.schema_url,
        dir.display()
    );
    Ok(())
}

/// The SHA-256 of the schema published at `schema_url`, downloaded now into a
/// scratch file under `.spec-cache/` (removed afterwards).
fn published_schema_hash(root: &Path, lock: &Lock) -> Result<String, String> {
    let scratch = root.join(".spec-cache");
    std::fs::create_dir_all(&scratch).map_err(|e| format!("{}: {e}", scratch.display()))?;
    sweep_scratch(&scratch);
    let published = scratch.join(format!(".published-{}.json", std::process::id()));
    let hash = download(&lock.schema_url, &published)
        .and_then(|()| sha256::file_hex(&published).map_err(|e| e.to_string()));
    let _ = std::fs::remove_file(&published);
    hash
}

/// `cargo xtask spec-published`: fail if the schema published at
/// `schema_url` today is not byte-identical to the pinned one
/// (`schema_sha256`). The scheduled CI job runs this, because `spec-fetch`
/// only ever downloads the pinned commit and so cannot see upstream
/// republishing 1.0.
pub fn spec_published(root: &Path) -> Result<(), String> {
    let lock = Lock::read(root)?;
    lock.require_hashes()?;
    let published = published_schema_hash(root, &lock)?;
    if published != lock.schema_sha256 {
        return Err(format!(
            "the schema published at {} (sha256 {published}) differs from the pinned one \
             (spec.lock schema_sha256 {}, commit {}): upstream changed the published 1.0 \
             schema; review it and pin the matching commit with `cargo xtask spec-bump <sha>`",
            lock.schema_url, lock.schema_sha256, lock.commit
        ));
    }
    println!(
        "{} matches the pinned schema (sha256 {published}, commit {})",
        lock.schema_url, lock.commit
    );
    Ok(())
}

fn download(url: &str, dest: &Path) -> Result<(), String> {
    let mut curl = Command::new("curl");
    curl.args(["--silent", "--show-error", "--fail", "--location"])
        .args(["--proto", "=https", "--retry", "3", "--output"])
        .arg(dest)
        .arg(url);
    run(&mut curl, "curl").map_err(|e| format!("downloading {url}: {e}"))
}

fn run(cmd: &mut Command, name: &str) -> Result<(), String> {
    let status = cmd
        .status()
        .map_err(|e| format!("cannot run {name}: {e}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("{name} failed ({status})"))
    }
}

/// Moves `staging` to `dest`, replacing any existing `dest`. The old `dest`
/// is moved aside first and put back if `staging` cannot be moved in; if
/// even that fails, the error names where the previous cache remains.
fn replace_dir(staging: &Path, dest: &Path) -> Result<(), String> {
    let name = dest
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or("spec cache directory has no name")?;
    let old = dest.with_file_name(format!(".old-{name}-{}", std::process::id()));
    remove(&old)?;
    let had_old = match std::fs::rename(dest, &old) {
        Ok(()) => true,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => false,
        Err(e) => return Err(format!("{}: {e}", dest.display())),
    };
    if let Err(e) = std::fs::rename(staging, dest) {
        if had_old {
            if let Err(e2) = std::fs::rename(&old, dest) {
                return Err(format!(
                    "{}: {e}; restoring the previous cache also failed ({e2}); it remains at {}",
                    dest.display(),
                    old.display()
                ));
            }
        }
        return Err(format!("{}: {e}", dest.display()));
    }
    let _ = remove(&old);
    Ok(())
}

/// Best-effort removal of the scratch entries (`.archive-*`, `.staging-*`,
/// `.old-*`, `.published-*`) that an earlier run killed mid-fetch left in
/// `base`. Each name ends in its run's pid; the current run's are left alone.
fn sweep_scratch(base: &Path) {
    let Ok(entries) = std::fs::read_dir(base) else {
        return;
    };
    let me = std::process::id();
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if ![".archive-", ".staging-", ".old-", ".published-"]
            .iter()
            .any(|p| name.starts_with(p))
        {
            continue;
        }
        let stem = name
            .strip_suffix(".tar.gz")
            .or_else(|| name.strip_suffix(".json"))
            .unwrap_or(name);
        let pid = stem
            .rsplit_once('-')
            .and_then(|(_, pid)| pid.parse::<u32>().ok());
        if pid.map_or(true, |pid| pid == me) {
            continue;
        }
        let path = entry.path();
        if entry.file_type().is_ok_and(|t| t.is_dir()) {
            let _ = std::fs::remove_dir_all(&path);
        } else {
            let _ = std::fs::remove_file(&path);
        }
    }
}

fn remove(path: &Path) -> Result<(), String> {
    match std::fs::remove_dir_all(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(format!("{}: {e}", path.display())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn failed_replace_keeps_the_existing_cache() -> Result<(), String> {
        let tmp = std::env::temp_dir().join(format!("xtask-replace-{}", std::process::id()));
        let _ = remove(&tmp);
        let dest = tmp.join("cache");
        std::fs::create_dir_all(&dest).map_err(|e| e.to_string())?;
        std::fs::write(dest.join("sentinel"), "x").map_err(|e| e.to_string())?;
        // The staging directory does not exist, so moving it into place fails.
        let result = replace_dir(&tmp.join(".staging"), &dest);
        let sentinel = dest.join("sentinel").exists();
        let leftovers: Vec<_> = std::fs::read_dir(&tmp)
            .map_err(|e| e.to_string())?
            .filter_map(|e| e.ok())
            .map(|e| e.file_name())
            .collect();
        let _ = remove(&tmp);
        assert!(result.is_err());
        assert!(sentinel, "the existing cache was lost");
        assert_eq!(leftovers, ["cache"], "scratch directories left behind");
        Ok(())
    }

    #[test]
    fn replace_swaps_in_the_new_tree() -> Result<(), String> {
        let tmp = std::env::temp_dir().join(format!("xtask-swap-{}", std::process::id()));
        let _ = remove(&tmp);
        let (dest, staging) = (tmp.join("cache"), tmp.join(".staging"));
        std::fs::create_dir_all(&dest).map_err(|e| e.to_string())?;
        std::fs::create_dir_all(&staging).map_err(|e| e.to_string())?;
        std::fs::write(dest.join("old"), "x").map_err(|e| e.to_string())?;
        std::fs::write(staging.join("new"), "y").map_err(|e| e.to_string())?;
        let result = replace_dir(&staging, &dest);
        let (old, new) = (dest.join("old").exists(), dest.join("new").exists());
        let count = std::fs::read_dir(&tmp).map_err(|e| e.to_string())?.count();
        let _ = remove(&tmp);
        result?;
        assert!(!old && new);
        assert_eq!(count, 1, "scratch directories left behind");
        Ok(())
    }

    #[test]
    fn tree_digest_is_order_independent_and_content_sensitive() -> Result<(), String> {
        let base = std::env::temp_dir().join(format!("xtask-tree-{}", std::process::id()));
        let _ = remove(&base);
        std::fs::create_dir_all(base.join("d/e")).map_err(|e| e.to_string())?;
        std::fs::write(base.join("f.json"), "1").map_err(|e| e.to_string())?;
        std::fs::write(base.join("d/x"), "2").map_err(|e| e.to_string())?;
        std::fs::write(base.join("d/e/y"), "3").map_err(|e| e.to_string())?;
        let paths = ["f.json".to_owned(), "d".to_owned()];
        let a = tree_digest(&base, &paths)?;
        let b = tree_digest(&base, &[paths[1].clone(), paths[0].clone()])?;
        std::fs::write(base.join("d/e/y"), "4").map_err(|e| e.to_string())?;
        let c = tree_digest(&base, &paths)?;
        let missing = tree_digest(&base, &["nope".to_owned()]);
        remove(&base)?;
        assert_eq!(a, b);
        assert_ne!(a, c);
        assert!(missing.is_err());
        Ok(())
    }

    #[test]
    fn spec_bump_unreachable_published_schema_changes_nothing() -> Result<(), String> {
        let ws = Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
        let lock_text = std::fs::read_to_string(ws.join(lock::FILE)).map_err(|e| e.to_string())?;
        let commit = Lock::parse(&lock_text)?.commit;
        let tmp = std::env::temp_dir().join(format!("xtask-bump-{}", std::process::id()));
        let _ = remove(&tmp);
        let cache = tmp.join("cache");
        std::fs::create_dir_all(&cache).map_err(|e| e.to_string())?;
        // Connection refused: fast and deterministic.
        let lock_text =
            lock::set_string(&lock_text, "schema_url", "https://127.0.0.1:1/schema.json")?;
        std::fs::write(tmp.join(lock::FILE), &lock_text).map_err(|e| e.to_string())?;
        let old = "0".repeat(40);
        std::fs::write(cache.join(STAMP), format!("{old}\n")).map_err(|e| e.to_string())?;
        std::fs::write(cache.join("sentinel"), "x").map_err(|e| e.to_string())?;

        std::env::set_var(SPEC_DIR_ENV, &cache);
        let result = spec_bump(&tmp, &commit);
        std::env::remove_var(SPEC_DIR_ENV);

        let stamp = std::fs::read_to_string(cache.join(STAMP)).unwrap_or_default();
        let sentinel = cache.join("sentinel").exists();
        let lock_after =
            std::fs::read_to_string(tmp.join(lock::FILE)).map_err(|e| e.to_string())?;
        let _ = remove(&tmp);
        assert!(result.is_err(), "bump must refuse");
        assert_eq!(stamp.trim(), old, "pinned cache stamp was replaced");
        assert!(sentinel, "pinned cache was replaced");
        assert_eq!(lock_after, lock_text, "spec.lock changed");
        Ok(())
    }

    #[test]
    fn scratch_left_by_an_interrupted_run_is_swept() -> Result<(), String> {
        let ws = Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
        let lock_text = std::fs::read_to_string(ws.join(lock::FILE)).map_err(|e| e.to_string())?;
        // Connection refused: fast and deterministic.
        let lock_text =
            lock::set_string(&lock_text, "schema_url", "https://127.0.0.1:1/schema.json")?;
        let lock = Lock::parse(&lock_text)?;
        let tmp = std::env::temp_dir().join(format!("xtask-sweep-{}", std::process::id()));
        let _ = remove(&tmp);
        let base = tmp.join(".spec-cache");
        let commit = "2".repeat(40);
        // pid 1 is never an xtask run.
        let stale_dirs = [format!(".staging-{commit}-1"), format!(".old-{commit}-1")];
        let stale_files = [
            format!(".archive-{commit}-1.tar.gz"),
            ".published-1.json".to_owned(),
        ];
        let kept = [commit.clone(), ".staging-notapid".to_owned()];
        for d in stale_dirs.iter().chain(&kept) {
            std::fs::create_dir_all(base.join(d)).map_err(|e| e.to_string())?;
        }
        for f in &stale_files {
            std::fs::write(base.join(f), "x").map_err(|e| e.to_string())?;
        }

        let result = published_schema_hash(&tmp, &lock);

        let left: Vec<String> = stale_dirs
            .iter()
            .chain(&stale_files)
            .filter(|n| base.join(n).exists())
            .cloned()
            .collect();
        let kept_ok = kept.iter().all(|n| base.join(n).exists());
        let _ = remove(&tmp);
        assert!(result.is_err());
        assert!(left.is_empty(), "stale scratch survived: {left:?}");
        assert!(kept_ok, "a non-scratch entry was removed");
        Ok(())
    }

    /// Hermetic: a local archive whose schema differs from the published
    /// hash must be refused before the existing cache is touched.
    #[test]
    fn schema_mismatch_refuses_before_replacing_cache() -> Result<(), String> {
        let tmp = std::env::temp_dir().join(format!("xtask-mismatch-{}", std::process::id()));
        let _ = remove(&tmp);
        let commit = "1".repeat(40);
        let src = tmp.join("src").join(format!("repo-{commit}"));
        std::fs::create_dir_all(src.join("spec")).map_err(|e| e.to_string())?;
        std::fs::write(src.join("spec/schema.json"), "{}").map_err(|e| e.to_string())?;
        let archive = tmp.join("a.tar.gz");
        let mut t = Command::new("tar");
        t.arg("-czf")
            .arg(&archive)
            .arg("-C")
            .arg(tmp.join("src"))
            .arg(format!("repo-{commit}"));
        run(&mut t, "tar")?;
        let lock_text = format!(
            "upstream = \"https://github.com/o/repo\"\ncommit = \"{commit}\"\n\
             sha256 = \"{z}\"\ntree_sha256 = \"{z}\"\nschema_url = \"https://example.invalid/s.json\"\n\
             schema_sha256 = \"{z}\"\npaths = [\n  \"spec/schema.json\",\n]\n",
            z = "0".repeat(64)
        );
        let lock = Lock::parse(&lock_text)?;
        let dest = tmp.join("cache");
        std::fs::create_dir_all(&dest).map_err(|e| e.to_string())?;
        std::fs::write(dest.join(STAMP), "old\n").map_err(|e| e.to_string())?;
        std::fs::write(dest.join("sentinel"), "x").map_err(|e| e.to_string())?;

        let wrong = "f".repeat(64);
        let result = install_archive(&lock, &commit, None, Some(&wrong), &archive, &dest);
        let stamp = std::fs::read_to_string(dest.join(STAMP)).unwrap_or_default();
        let sentinel = dest.join("sentinel").exists();
        let _ = remove(&tmp);
        let err = result.err().unwrap_or_default();
        assert!(err.contains("refusing to pin"), "unexpected: {err}");
        assert_eq!(stamp, "old\n");
        assert!(sentinel);
        Ok(())
    }
}
