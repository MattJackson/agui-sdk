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
/// cargo 1.75 reads).
pub fn msrv_lock(root: &Path) -> Result<(), String> {
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_owned());
    let version = Command::new(&cargo)
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
    let status = Command::new(&cargo)
        .arg("update")
        .current_dir(root)
        .env("CARGO_RESOLVER_INCOMPATIBLE_RUST_VERSIONS", "fallback")
        .status()
        .map_err(|e| format!("cannot run {cargo}: {e}"))?;
    if !status.success() {
        return Err(format!("cargo update failed ({status})"));
    }
    let lock = std::fs::read_to_string(root.join("Cargo.lock")).map_err(|e| e.to_string())?;
    if lock.lines().any(|l| l.trim() == "version = 3") {
        Ok(())
    } else {
        Err("Cargo.lock is no longer lockfile version 3; cargo 1.75 cannot read it".to_owned())
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
