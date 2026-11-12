//! Repository tasks for `agui-sdk`, behind `cargo xtask` (see `main.rs`).
//!
//! No dependencies: downloads go through `curl` and `tar`, SHA-256 is
//! computed in-process, `spec.lock` is parsed by hand, and the generator
//! reads JSON with its own small reader. The crate builds on the
//! workspace MSRV (1.75), like everything else in the workspace.

#![forbid(unsafe_code)]

pub mod codegen;
pub mod fetch;
pub mod lock;
pub mod sha256;

use std::path::Path;
use std::process::Command;

use lock::Lock;

/// `cargo xtask codegen`: regenerates both tables from the pinned cache.
/// With `check`, writes nothing and fails if a committed file differs.
pub fn codegen(root: &Path, check: bool) -> Result<(), String> {
    let lock = Lock::read(root)?;
    let cache = fetch::pinned_cache(root, &lock)?;
    let outputs = codegen::generate(&cache, &lock.commit)?;
    let mut stale = Vec::new();
    for out in &outputs {
        let path = root.join(out.path);
        let current = match std::fs::read_to_string(&path) {
            Ok(s) => s,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(e) if check => return Err(format!("{}: {e}", path.display())),
            Err(_) => String::new(),
        };
        if current == out.contents {
            continue;
        }
        if check {
            stale.push(out.path);
        } else {
            std::fs::write(&path, &out.contents).map_err(|e| format!("{}: {e}", path.display()))?;
            println!("wrote {}", out.path);
        }
    }
    if stale.is_empty() {
        if check {
            println!("generated tables match spec {}", lock.commit);
        }
        Ok(())
    } else {
        Err(format!(
            "generated files differ from what spec {} produces: {}; run 'cargo xtask codegen'",
            lock.commit,
            stale.join(", ")
        ))
    }
}

/// `cargo xtask msrv-lock`: re-resolves `Cargo.lock` with
/// `CARGO_RESOLVER_INCOMPATIBLE_RUST_VERSIONS=fallback` (needs cargo >= 1.84),
/// so every locked version builds on the workspace `rust-version`, and
/// fails unless the result is still lockfile version 3 (the newest format
/// cargo 1.75 reads). On failure the previous `Cargo.lock` is put back.
pub fn msrv_lock(root: &Path) -> Result<(), String> {
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_owned());
    msrv_lock_with(root, &cargo)
}

fn msrv_lock_with(root: &Path, cargo: &str) -> Result<(), String> {
    let version = Command::new(cargo)
        .arg("--version")
        .output()
        .map_err(|e| format!("cannot run {cargo}: {e}"))?;
    let version = String::from_utf8_lossy(&version.stdout).into_owned();
    let minor = version
        .split_whitespace()
        .nth(1)
        .and_then(|v| v.split('.').nth(1))
        .and_then(|m| m.parse::<u32>().ok())
        .ok_or_else(|| format!("cannot read the cargo version from {version:?}"))?;
    if minor < 84 {
        return Err(format!(
            "msrv-lock needs cargo >= 1.84 for the fallback resolver; found {}",
            version.trim()
        ));
    }
    let lock_path = root.join("Cargo.lock");
    // Only a missing lockfile may be removed again on failure; any other read
    // error stops before cargo touches the file.
    let original = match std::fs::read(&lock_path) {
        Ok(bytes) => Some(bytes),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(format!("cannot read {}: {e}", lock_path.display())),
    };
    let Err(err) = update_lock(root, cargo, &lock_path) else {
        return Ok(());
    };
    // A refused or half-written lockfile must not be left behind.
    let restored = match &original {
        Some(bytes) => std::fs::write(&lock_path, bytes),
        None => std::fs::remove_file(&lock_path).or_else(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                Ok(())
            } else {
                Err(e)
            }
        }),
    };
    match restored {
        Ok(()) => Err(format!("{err}; Cargo.lock left unchanged")),
        Err(e) => Err(format!("{err}; restoring Cargo.lock failed: {e}")),
    }
}

fn update_lock(root: &Path, cargo: &str, lock_path: &Path) -> Result<(), String> {
    let status = Command::new(cargo)
        .arg("update")
        .current_dir(root)
        .env("CARGO_RESOLVER_INCOMPATIBLE_RUST_VERSIONS", "fallback")
        .status()
        .map_err(|e| format!("cannot run {cargo}: {e}"))?;
    if !status.success() {
        return Err(format!("cargo update failed ({status})"));
    }
    let lock = std::fs::read_to_string(lock_path).map_err(|e| e.to_string())?;
    if lock.lines().any(|l| l.trim() == "version = 3") {
        Ok(())
    } else {
        Err(
            "cargo update wrote a Cargo.lock that is not lockfile version 3, which cargo 1.75 \
             cannot read"
                .to_owned(),
        )
    }
}

/// `cargo xtask trace`: the traceability gate in its release form,
/// `agui-trace gate --no-pending` (nothing may remain pending).
pub fn trace(root: &Path) -> Result<(), String> {
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_owned());
    let status = Command::new(&cargo)
        .args(["run", "--quiet", "--locked", "-p", "agui-test-support"])
        .args(["--bin", "agui-trace", "--", "gate", "--no-pending"])
        .current_dir(root)
        .status()
        .map_err(|e| format!("cannot run {cargo}: {e}"))?;
    if status.success() {
        Ok(())
    } else {
        Err("the traceability gate failed".to_owned())
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    /// A stand-in cargo that reports 1.90 and writes a version 4 lockfile.
    const FAKE_CARGO: &str = "#!/bin/sh\n\
        if [ \"$1\" = --version ]; then echo 'cargo 1.90.0 (x 2025-01-01)'; exit 0; fi\n\
        printf 'version = 4\\n' > Cargo.lock\n";

    #[test]
    fn refused_lockfile_is_restored() -> Result<(), String> {
        use std::os::unix::fs::PermissionsExt as _;
        let tmp = std::env::temp_dir().join(format!("xtask-msrv-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).map_err(|e| e.to_string())?;
        let cargo = tmp.join("fake-cargo");
        std::fs::write(&cargo, FAKE_CARGO).map_err(|e| e.to_string())?;
        std::fs::set_permissions(&cargo, std::fs::Permissions::from_mode(0o755))
            .map_err(|e| e.to_string())?;
        let original = "# original\nversion = 3\n";
        std::fs::write(tmp.join("Cargo.lock"), original).map_err(|e| e.to_string())?;

        let result = msrv_lock_with(&tmp, &cargo.to_string_lossy());
        let after = std::fs::read_to_string(tmp.join("Cargo.lock")).unwrap_or_default();
        let _ = std::fs::remove_dir_all(&tmp);
        let err = result.err().unwrap_or_default();
        assert!(err.contains("lockfile version 3"), "{err}");
        assert!(err.ends_with("; Cargo.lock left unchanged"), "{err}");
        assert_eq!(after, original, "the refused Cargo.lock was left in place");
        Ok(())
    }

    /// A stand-in cargo that replaces `Cargo.lock` with a directory and fails,
    /// so restoring the original cannot succeed.
    const FAKE_CARGO_DIR: &str = "#!/bin/sh\n\
        if [ \"$1\" = --version ]; then echo 'cargo 1.90.0 (x 2025-01-01)'; exit 0; fi\n\
        rm -f Cargo.lock; mkdir Cargo.lock; exit 1\n";

    #[test]
    fn failed_restore_does_not_claim_lockfile_unchanged() -> Result<(), String> {
        use std::os::unix::fs::PermissionsExt as _;
        let tmp = std::env::temp_dir().join(format!("xtask-msrv-dir-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).map_err(|e| e.to_string())?;
        let cargo = tmp.join("fake-cargo");
        std::fs::write(&cargo, FAKE_CARGO_DIR).map_err(|e| e.to_string())?;
        std::fs::set_permissions(&cargo, std::fs::Permissions::from_mode(0o755))
            .map_err(|e| e.to_string())?;
        std::fs::write(tmp.join("Cargo.lock"), "version = 3\n").map_err(|e| e.to_string())?;

        let result = msrv_lock_with(&tmp, &cargo.to_string_lossy());
        let _ = std::fs::remove_dir_all(&tmp);
        let err = result.err().unwrap_or_default();
        assert!(err.contains("restoring Cargo.lock failed"), "{err}");
        assert!(!err.contains("left unchanged"), "{err}");
        Ok(())
    }

    /// A stand-in cargo whose `update` fails without touching anything.
    const FAKE_CARGO_FAIL: &str = "#!/bin/sh\n\
        if [ \"$1\" = --version ]; then echo 'cargo 1.90.0 (x 2025-01-01)'; exit 0; fi\n\
        exit 1\n";

    #[test]
    fn unreadable_lockfile_is_not_deleted() -> Result<(), String> {
        use std::os::unix::fs::PermissionsExt as _;
        let tmp = std::env::temp_dir().join(format!("xtask-msrv-perm-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).map_err(|e| e.to_string())?;
        let cargo = tmp.join("fake-cargo");
        std::fs::write(&cargo, FAKE_CARGO_FAIL).map_err(|e| e.to_string())?;
        std::fs::set_permissions(&cargo, std::fs::Permissions::from_mode(0o755))
            .map_err(|e| e.to_string())?;
        let lock = tmp.join("Cargo.lock");
        std::fs::write(&lock, "version = 3\n").map_err(|e| e.to_string())?;
        std::fs::set_permissions(&lock, std::fs::Permissions::from_mode(0o000))
            .map_err(|e| e.to_string())?;
        if std::fs::read(&lock).is_ok() {
            // Running with privileges that ignore file modes; nothing to test.
            let _ = std::fs::remove_dir_all(&tmp);
            return Ok(());
        }

        let result = msrv_lock_with(&tmp, &cargo.to_string_lossy());
        let still_there = lock.exists();
        let _ = std::fs::set_permissions(&lock, std::fs::Permissions::from_mode(0o644));
        let _ = std::fs::remove_dir_all(&tmp);
        let err = result.err().unwrap_or_default();
        assert!(still_there, "the unreadable Cargo.lock was deleted: {err}");
        assert!(!err.contains("left unchanged"), "{err}");
        assert!(err.contains("Cargo.lock"), "{err}");
        Ok(())
    }
}
