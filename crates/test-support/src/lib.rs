//! Test harness for `agui-sdk`. Unpublished; a dev-dependency only.
//!
//! * [`spec`]: finds the upstream spec data that `cargo xtask spec-fetch`
//!   extracted for the commit pinned in `spec.lock`. A missing cache is a
//!   visible skip (a failure under `AGUI_REQUIRE_SPEC=1`); a cache for the
//!   wrong commit is always a failure. Nothing from upstream is vendored.
//! * [`corpus`]: discovers the stream, value and protobuf-byte corpora,
//!   checks the stream and value corpora against their upstream
//!   `MANIFEST.txt` in both directions (the byte corpus, which has none, by
//!   pairing each case with a valid value document), and reports counts
//!   without pinning them.
//! * [`oracle`]: runs a stream fixture through the client pipeline and judges
//!   the result against the fixture's `expect` block. Unknown expectation keys
//!   fail closed.
//! * [`overrides`]: the Rust lane's override file
//!   (`tests/corpus/rust-lane-overrides.toml`), each entry with its required
//!   `intentional` reason.
//! * [`producer`]: this repository's producer fixtures
//!   (`tests/fixtures/producer/`).
//! * [`lane`]: runs a check over a whole corpus and reports every failure.
//! * [`mock`]: a scripted in-process [`Transport`](agui_sdk::client::Transport).
//! * [`http_mock`]: the HTTP transport's test peer: a mock server with
//!   delayed heads and TLS (optionally requiring client certificates), a
//!   forward proxy (`CONNECT` and absolute-form), and the test PKI.
//! * [`alloc`]: a counting global allocator for benches and allocation tests.
//! * [`strategies`]: `proptest` strategies for events and streams.
//! * [`traceability`]: the `/// covers: ID` gate against `docs/SPEC-MAP.md`
//!   and `docs/compliance/compliance.toml`, and the progress report.

// `unsafe` is denied everywhere except the counting allocator, where
// implementing `GlobalAlloc` requires it. The published crates forbid it.
#![deny(unsafe_code)]
#![deny(missing_docs)]

pub mod alloc;
pub mod corpus;
pub mod http_mock;
pub mod lane;
pub mod mock;
pub mod oracle;
pub mod overrides;
pub mod producer;
pub mod spec;
pub mod strategies;
pub mod traceability;

/// The workspace root (the directory holding `spec.lock`).
pub fn workspace_root() -> std::path::PathBuf {
    let here = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    here.ancestors()
        .find(|dir| dir.join("spec.lock").is_file())
        .unwrap_or(here)
        .to_path_buf()
}
