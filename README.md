# agui-sdk

[![Sponsor](https://img.shields.io/badge/Sponsor-%E2%9D%A4-ea4aaa?logo=github-sponsors)](https://github.com/sponsors/MattJackson)

[![CI](https://github.com/MattJackson/agui-sdk/actions/workflows/dev.yml/badge.svg?branch=dev)](https://github.com/MattJackson/agui-sdk/actions/workflows/dev.yml)
[![crates.io](https://img.shields.io/crates/v/agui-sdk.svg)](https://crates.io/crates/agui-sdk)
[![docs.rs](https://img.shields.io/docsrs/agui-sdk)](https://docs.rs/agui-sdk)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](#license)
[![MSRV 1.75](https://img.shields.io/badge/MSRV-1.75-blue.svg)](#minimum-supported-rust-version)
[![unsafe: forbidden](https://img.shields.io/badge/unsafe-forbidden-brightgreen.svg)](#design)
<!-- default deps: `default = []`; `cargo tree -p agui-sdk -e normal --depth 1` lists exactly serde and serde_json. Not CI-enforced: change the badge if that changes. -->
[![default deps: serde + serde_json](https://img.shields.io/badge/default%20deps-serde%20%2B%20serde__json-brightgreen.svg)](#feature-flags)
[![OpenSSF Scorecard](https://api.securityscorecards.dev/projects/github.com/MattJackson/agui-sdk/badge)](https://scorecard.dev/viewer/?uri=github.com/MattJackson/agui-sdk)
[![REUSE status](https://api.reuse.software/badge/github.com/MattJackson/agui-sdk)](https://api.reuse.software/info/github.com/MattJackson/agui-sdk)

<!-- OpenSSF Best Practices. Kept commented out until the owner registers the
     project at https://www.bestpractices.dev (docs/SETUP.md, step 6) and the
     site assigns a numeric project id. Replace NNNNN with that id, and put the
     line back directly after the unsafe badge above, which is where it sits in
     the owner's other repositories:
[![OpenSSF Best Practices](https://www.bestpractices.dev/projects/NNNNN/badge)](https://www.bestpractices.dev/projects/NNNNN)
-->

<!-- The Scorecard and REUSE badges read "unknown" until the first scheduled
     scorecard.yml run and the first time api.reuse.software scans a pushed
     branch. Both go green on their own; neither needs an account.

     The static badges are claims this file has to keep true:
     - `MSRV 1.75` is `rust-version` in Cargo.toml. qa.yml's manifest job fails
       if this badge, clippy.toml and Cargo.toml disagree, and the msrv job
       builds and tests on exactly that toolchain.
     - `unsafe: forbidden` is `#![forbid(unsafe_code)]` at the root of every
       library crate in the workspace. qa.yml's lint job checks each one.
     If either ever stops being true, the badge is the thing to delete first.

     Deliberately absent: a "dependencies: 0" badge. This crate has
     dependencies (serde, serde_json, and more behind features), so it would be
     false. -->

<!-- Codecov. The upload is wired into qa.yml's coverage job and starts as soon
     as the CODECOV_TOKEN secret exists (docs/SETUP.md). Uncomment on the
     first green upload:
[![codecov](https://codecov.io/gh/MattJackson/agui-sdk/graph/badge.svg)](https://codecov.io/gh/MattJackson/agui-sdk)
-->

A **Rust SDK for the [AG-UI](https://docs.ag-ui.com) 1.0 protocol**: the
event-based protocol that connects AI agents to user-facing applications.

AG-UI standardises what flows between an agent backend and a frontend: a run's
lifecycle (`RUN_STARTED` … `RUN_FINISHED`), streamed text messages, tool
calls, reasoning, and shared state as snapshots and JSON Patch deltas. It is
transport-agnostic, with Server-Sent Events and a protobuf binary encoding as
the standard wires. The protocol is documented at <https://docs.ag-ui.com>, and
its normative specification (JSON Schema plus conformance fixtures) lives in
the [AG-UI repository](https://github.com/ag-ui-protocol/ag-ui/tree/main/spec).

**This is an independent implementation. It is not affiliated with, endorsed
by, or maintained by the AG-UI project or its sponsors.** "AG-UI" refers to the
protocol it implements.

```toml
[dependencies]
# A client that calls agents over HTTPS (`rustls` implies `client`):
agui-sdk = { version = "0.9", features = ["rustls"] }
```

Pick the features for what you are writing: `["rustls"]` for a client,
`["axum"]` for an agent endpoint in an axum 0.8 app (`axum` implies `server`),
both for both. With no features you get the protocol types and the sans-IO
pipeline only. The `client` feature alone speaks plain `http://`; an
`https://` URL needs `rustls` (recommended) or `native-tls`, and `build()`
refuses it otherwise.

## What it provides

| | |
| --- | --- |
| **Protocol types** | `serde` types for every AG-UI 1.0 schema entry: events, messages, tools, run input and state. Received streams go through `pipeline::Processor` (the client uses it for you), which applies the spec's unknown-field and unknown-value rules; the derived `Deserialize` impls are plain decoding for trusted data and reject an unknown closed-set value. |
| **Conformant client** | Connect to any AG-UI agent endpoint, send `RunAgentInput`, and consume a validated, typed event stream. Enforces the run lifecycle (one run active at a time, sequential runs in one stream) and applies `STATE_SNAPSHOT` / `STATE_DELTA` and `MESSAGES_SNAPSHOT` for you. |
| **Conformant server** | Emit a well-formed event stream from your own agent, with the ordering rules checked as you emit rather than discovered by the client. |
| **SSE transport** | Encode and decode `text/event-stream`. |
| **Protobuf transport** | The binary wire encoding (`application/vnd.ag-ui.event+proto`). |

### Feature flags

<!-- This table mirrors the feature table in src/lib.rs and Cargo.toml
     [features] (checked: 11 optional features, default = []); keep the three
     in step. -->

| Feature | What it adds | Extra dependencies |
| --- | --- | --- |
| *(default)* | Protocol types, diagnostics, errors, bounded JSON Patch, the sans-IO pipeline | `serde`, `serde_json` |
| `client` | Async HTTP client over hyper (HTTP/1.1, plain `http://` only: add `rustls` or `native-tls` for HTTPS): `AgentClient`, `Thread`, SSE transport, middleware, observers; needs a tokio runtime | `hyper`, `hyper-util`, `http`, `http-body(-util)`, `tokio` (`time`, `net`), `tower-service`, `bytes`, `memchr`, `pin-project-lite` |
| `rustls` | HTTPS via rustls (ring, webpki-roots); implies `client` | `hyper-rustls`, `rustls`, `webpki-roots` |
| `rustls-native-roots` | Also trust the OS root store; implies `rustls` | `rustls-native-certs` |
| `native-tls` | HTTPS via the platform stack; implies `client` (needs Rust 1.80, see [MSRV](#minimum-supported-rust-version)) | `hyper-tls`, `native-tls`, `tokio-native-tls` |
| `server` | Runtime-agnostic producer: `Agent`, `RunContext`, `Runner`, encoders, framework-free `server::http`; no runtime | `bytes`, `pin-project-lite` |
| `axum` | `FromRequest`/`IntoResponse` types for axum 0.8; implies `server` | `axum-core` 0.5, `http`, `http-body(-util)`, `tokio` (`time`, `net`) |
| `proto` | Protobuf wire (`application/vnd.ag-ui.event+proto`) for client and server | `bytes` |
| `stream` | `futures_core::Stream` impls for the crate's streams | `futures-core` |
| `preserve-order` | Insertion-ordered JSON objects (protobuf byte parity from typed events) | `indexmap` (via `serde_json`) |
| `tracing` / `log` | Route diagnostics to `tracing` or `log` | `tracing` / `log` |

Public dependencies are `serde` 1 (its traits: protocol types implement
`Serialize`/`Deserialize`, and a few signatures take `Serialize` or
`DeserializeOwned` bounds) and `serde_json` 1 (`Value`). Every other
dependency is private, except under the opt-in `stream` and `axum` features,
which exist to implement `futures_core::Stream` and axum-core's
`FromRequest`/`IntoResponse`.

### Building on Rust 1.75

The MSRV is 1.75 for every feature except `native-tls` (1.80), but cargo 1.75 cannot pick
1.75-compatible dependency versions by itself. Generate the lockfile with a
cargo ≥ 1.84 and build with 1.75:

```sh
CARGO_RESOLVER_INCOMPATIBLE_RUST_VERSIONS=fallback cargo update
cargo +1.75 build --locked
```

## Compliance

`agui-sdk` claims conformance to **AG-UI 1.0**. The claim is checked, not
asserted: every normative requirement in the specification has an ID, every ID
is mapped to the tests that cover it (`/// covers: ID` in the test source,
indexed in `docs/SPEC-MAP.md`), and on every qa run the upstream schema and
fixtures are fetched fresh and the conformance suite is run against them.
[`docs/CONFORMANCE.md`](docs/CONFORMANCE.md) says exactly how the claim is
proven, what it does not cover, and where to see the latest result.

## Quick start

Both examples are compiled as doctests of the crate, so they match the API
of this release.

Consume an agent's event stream (features `["rustls"]`):

```rust,no_run
use agui_sdk::client::{AgentClient, RunRequest};
use agui_sdk::Event;

async fn chat() -> agui_sdk::Result<()> {
    let client = AgentClient::http("https://agent.example.com/run").build()?;
    let mut run = client.run(RunRequest::new().user("Hello"));

    while let Some(update) = run.next().await {
        if let Event::TextMessageContent(e) = &*update?.event {
            print!("{}", e.delta);
        }
    }
    Ok(())
}
```

Serve one by implementing `Agent` and mounting it in an axum 0.8 router
(features `["axum"]`, plus `axum = "0.8"` in your own dependencies):

```rust
use agui_sdk::server::axum::{respond, AguiRequest};
use agui_sdk::server::http::ServeConfig;
use agui_sdk::server::{Agent, BoxError, RunContext, RunOutcome, Runner};

struct Echo;

impl Agent for Echo {
    type State = serde_json::Value;

    async fn run(&self, ctx: RunContext) -> Result<RunOutcome, BoxError> {
        ctx.emitter().say("Hello from the agent").await?;
        Ok(RunOutcome::success())
    }
}

let runner = Runner::new(Echo);
let serve = ServeConfig::default();
let app: axum::Router = axum::Router::new().route(
    "/agent",
    axum::routing::post(move |req: AguiRequest| async move { respond(&runner, &serve, req) }),
);
```

## Design

- `#![forbid(unsafe_code)]` in every library crate.
- Strict on what it sends, forgiving where the spec says to be on what it
  receives: output always validates against the 1.0 schema; input with unknown
  fields from a newer minor protocol version is accepted as the spec directs,
  and malformed input is an error, never a panic.
- Peer input is untrusted. Parsers are fuzzed, and a perf gate holds them to
  linear time and to an allocation budget ([`docs/PERFORMANCE.md`](docs/PERFORMANCE.md)).
- Minimal dependencies: the default build is the types plus `serde` and
  `serde_json`; everything else is behind a feature, and every dependency is
  justified in writing.

## `ag-ui-sdk` alias

[`ag-ui-sdk`](https://crates.io/crates/ag-ui-sdk) is published alongside this
crate as an alias: it re-exports `agui-sdk` unchanged, with the same features,
and is versioned and released in lockstep from the same tag. Depend on
whichever name you find first. If you depend on both, keep them at the same
version: `ag-ui-sdk` pins `agui-sdk` exactly (`=X.Y.Z`), so the build then has
one copy of the code, and a mismatched pair fails to resolve.

## Minimum supported Rust version

**Rust 1.75**, edition 2021, for every feature except `native-tls`, using a
lockfile resolved for 1.75 (see [Building on Rust 1.75](#building-on-rust-175)).
CI reads that floor from `Cargo.toml` rather than from a hardcoded value, and
covers it as follows:

- **Linux, macOS and Windows:** the library with no features is checked with
  warnings denied, and both crates are built and tested with default features.
  The tests also enable the test harness's `client`, `server` and `proto`.
- **Linux only:** every feature but `native-tls` together, checked and tested.
  The library with no features and with each feature on its own is checked at
  its floor with warnings denied; each feature is then tested on top of the
  harness's `client`, `server` and `proto`.
- **Clippy** runs on latest stable, on Linux, for the library with no features
  and with each feature on its own, and for the workspace with default and
  all features.

**native-tls: 1.80.** Its advisory-clean `openssl` / `openssl-sys` (0.10.80+ /
0.9.115+) need Rust 1.80, and the committed lock holds them; on Linux, CI
checks and clippies the library with `native-tls` alone, tests `native-tls`
plus the test harness's three features, and tests every feature together on
1.80. The older releases that build on 1.75 carry RUSTSEC-2026-0474, -0476 and
-0477, so this crate does not promise 1.75 for `native-tls`
(docs/DECISIONS.md D1).

<!-- Checked against qa.yml's msrv-features matrix (a no-features row and
     11 feature rows: native-tls on 1.80, the rest on 1.75), its msrv,
     msrv-all-features and lint jobs and their runs-on, and the MSRV column
     of src/lib.rs. If any feature's floor or any lane's OS changes, say so
     here and in src/lib.rs, docs/API.md, docs/TEST-PLAN.md and
     crates/ag-ui-sdk/README.md. -->

Raising the MSRV is not a breaking change under this policy, but it is never
done in a patch release, always gets a `CHANGELOG.md` entry, and is not done
lightly.

## Versioning

- The crate version is the crate's, not the protocol's. This crate implements
  **AG-UI protocol 1.0**; which protocol version a release implements is stated
  in its changelog entry.
- **0.9.x until 1.0.** 0.9.x releases are semver-compatible with one another,
  which `cargo-semver-checks` enforces in CI against the published crate. A
  change that would break that does not ship as 0.9.x.
- **1.0 is a maintainer decision**, not a date and not a feature count: it is
  made once the API has carried real agents and frontends without needing to
  change shape. [`ROADMAP.md`](ROADMAP.md) says what that means in practice.
- What changed in each release is in [`CHANGELOG.md`](CHANGELOG.md).

## Contributing

Contributions are welcome. [`CONTRIBUTING.md`](CONTRIBUTING.md) covers the
branch flow, the checks CI enforces, Conventional Commits, DCO sign-off and the
spec-traceability tags. [`GOVERNANCE.md`](GOVERNANCE.md) says who decides what,
and [`CODE_OF_CONDUCT.md`](CODE_OF_CONDUCT.md) applies everywhere.

## Security

Report vulnerabilities privately through
[GitHub private vulnerability reporting](https://github.com/MattJackson/agui-sdk/security/advisories/new),
never as a public issue. Scope, response times and process are in
[`SECURITY.md`](SECURITY.md); the argument for why the crate is adequately
secure, and its limits, is [`docs/ASSURANCE_CASE.md`](docs/ASSURANCE_CASE.md).

## Changelog

Notable changes are recorded in [`CHANGELOG.md`](CHANGELOG.md), which follows
[Keep a Changelog](https://keepachangelog.com) and
[Semantic Versioning](https://semver.org).

## License

MIT. See [`LICENSE`](LICENSE). The repository is
[REUSE](https://reuse.software)-compliant: `REUSE.toml` declares the licence of
every file. The upstream AG-UI specification data the conformance suite uses is
fetched at test time into a gitignored cache and is never committed or
published with this crate.
