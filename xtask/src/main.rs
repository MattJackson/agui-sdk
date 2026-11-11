//! `cargo xtask <task>`. Tasks:
//!
//! * `spec-fetch`: download the upstream archive at `spec.lock`'s `commit`,
//!   verify its `sha256`, extract `paths` into `.spec-cache/<commit>/`, and
//!   verify `tree_sha256` and `schema_sha256`. Does no network work when the
//!   cache already matches the lock.
//! * `spec-bump <sha>`: pin a new upstream commit. Fetches it, checks that
//!   the schema published at `schema_url` is byte-identical to the one at
//!   `<sha>`, rewrites `spec.lock`'s hashes, then runs `codegen`.
//! * `spec-published`: download the schema published at `schema_url` and
//!   fail unless it is byte-identical to the pinned `schema_sha256`
//!   (upstream drift; run on a schedule in CI).
//! * `codegen`: regenerate the enforcement shape table
//!   (`src/pipeline/shapes.rs`) from `schema.json` and the protobuf field
//!   table (`src/proto/fields.rs`) from the `.proto` files and
//!   `proto-freeze.txt`. Generated files carry `@generated` and the commit.
//! * `drift`: fail if regenerating would change the generated files.
//! * `trace`: the traceability gate (`docs/SPEC-MAP.md` vs `covers:` tags),
//!   in its release form: `agui-trace gate --no-pending`.
//! * `msrv-lock`: re-resolve `Cargo.lock` with the MSRV-aware (fallback)
//!   resolver, keeping lockfile version 3 so cargo 1.75 can read it.

#![forbid(unsafe_code)]

use std::process::ExitCode;

use xtask::lock::workspace_root;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let result = match args.first().map(String::as_str) {
        Some("spec-fetch") => workspace_root().and_then(|r| xtask::fetch::spec_fetch(&r)),
        Some("spec-bump") => match args.get(1) {
            Some(sha) => workspace_root().and_then(|root| {
                xtask::fetch::spec_bump(&root, sha)?;
                xtask::codegen(&root, false)
            }),
            None => Err("usage: cargo xtask spec-bump <sha>".to_owned()),
        },
        Some("spec-published") => workspace_root().and_then(|r| xtask::fetch::spec_published(&r)),
        Some("codegen") => workspace_root().and_then(|r| xtask::codegen(&r, false)),
        Some("drift") => workspace_root().and_then(|r| xtask::codegen(&r, true)),
        Some("trace") => workspace_root().and_then(|root| xtask::trace(&root)),
        Some("msrv-lock") => workspace_root().and_then(|root| xtask::msrv_lock(&root)),
        _ => {
            eprintln!(
                "usage: cargo xtask <spec-fetch|spec-bump <sha>|spec-published|codegen|drift|trace|msrv-lock>"
            );
            return ExitCode::from(2);
        }
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}
