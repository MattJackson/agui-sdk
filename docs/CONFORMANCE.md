# Conformance

`agui-sdk` claims conformance to **AG-UI protocol 1.0**. This document says
what that claim means, how it is proven, what it does not cover, and where to
see the current result. It is written so that a reader can check the claim
rather than take the badge's word for it.

The numbers below were measured locally on the commit named in
`spec.lock`. The stream-fixture count is the lane's target (68 fixtures, one
documented override); the other counts were run and verified. The authoritative
result is the `spec-sync` job's output on each qa run.

## What is claimed

1. **Every normative requirement** of the AG-UI 1.0 specification that applies
   to an SDK is implemented, and each one is covered by at least one test.
2. **Everything the crate emits validates** against the upstream 1.0 JSON
   Schema.
3. **Everything the specification's fixtures say to accept is accepted, and
   everything they say to reject is rejected**, with the behaviour the spec
   requires (an error, or a diagnostic for tolerated input such as unknown
   fields).
4. **Both standard wires agree**: an event stream round-trips through SSE and
   through the protobuf encoding to the same typed events, and the protobuf
   bytes match the upstream byte fixtures.

The specification is the AG-UI project's, at
<https://github.com/ag-ui-protocol/ag-ui/tree/main/spec>. This is an
independent implementation, not affiliated with that project; where the two
disagree, the specification wins.

## How it is proven

### 1. The upstream spec, pinned and fetched at test time

`spec.lock` pins an upstream commit and the sha256 of its archive, and lists
the paths used (the 1.0 schema, fixtures, conformance streams, the proto
definitions and byte fixtures). `cargo xtask spec-fetch` downloads exactly that,
verifies the hash, and extracts it into `.spec-cache/<commit>/`.

That directory is gitignored and excluded from the published crates (asserted
by qa.yml's `package` job), so no third-party material is ever committed or
redistributed. Bumping the pin is a deliberate change in its own pull request
(`cargo xtask spec-bump <sha>`), so a spec change and the code change it
requires are reviewed together.

- Pinned upstream commit: see `spec.lock` (`commit`).
- Fetch (CI and local): `cargo xtask spec-fetch`. Bump:
  `cargo xtask spec-bump <sha>`, which rewrites `spec.lock` and regenerates the
  tables (`cargo xtask codegen`); `cargo xtask drift` fails if the generated
  tables no longer match the pinned spec. Defined in `xtask/src/main.rs`.

### 2. Every requirement has an ID, and every ID has a test

[`SPEC-MAP.md`](SPEC-MAP.md) is the requirement ledger: each normative
statement in the spec, with a stable ID (`PROC-31`, `VER-11`, …), the spec
section it comes from, and the code that implements it. Tests name the IDs
they check:

```rust
/// covers: PROC-31
#[test]
fn second_run_started_before_finish_is_rejected() { /* ... */ }
```

A traceability check walks the test sources for `covers:` tags and compares
them with the ledger. It fails if:

- a requirement has no covering test;
- a tag names an ID that is not in the ledger;
- a tag is not on a test function that runs (no test attribute, or
  `#[ignore]`d); such a tag covers nothing;
- a requirement is marked not-applicable without a reason.

The check is the traceability gate:

```sh
cargo run -p agui-test-support --bin agui-trace -- gate --no-pending
```

It runs in the `spec-sync` job in qa.yml exactly so, with `--no-pending` and
the cache present (`cargo xtask trace` runs the same command), and
`agui-trace progress` regenerates `docs/compliance/progress.md`.

### 3. The fixture and stream suites

Run against `.spec-cache/`:

- **Schema fixtures**: every "valid" document deserialises and re-serialises
  to an equivalent document; every "invalid" one is rejected, or accepted with
  the diagnostic the spec requires.
- **Conformance streams**: complete event sequences, including multiple
  sequential runs, run through the client-side verifier and the state and
  message reducers; the resulting state and messages are compared with the
  expected outcome, and lifecycle violations must be rejected where the spec
  says so.
- **Emitted output**: every event the server side produces in the test suite
  is validated against the schema.
- **Protobuf bytes**: upstream byte fixtures decode to the expected events, and
  re-encode to the same bytes.

The suite **fails, never skips, when `.spec-cache/` is missing or empty**. A
conformance run that quietly skips is how a release claims conformance it never
checked.

The invocation, as run by qa.yml's `spec-sync` job with `AGUI_REQUIRE_SPEC=1`:

```sh
cargo xtask spec-fetch
cargo xtask drift
cargo test --locked --all-features -p agui-test-support
cargo test --locked --all-features \
  --test corpus_streams --test corpus_values --test corpus_proto_bytes \
  --test producer_fixtures --test traceability
cargo test --locked --workspace --all-features
cargo run --locked -p agui-test-support --bin agui-trace -- gate
```

## Where to see it

- **Every qa run and every release**: the `spec-sync (upstream AG-UI spec
  conformance)` job in the `qa full gate` workflow, which the release workflow
  re-runs on the tagged commit before publishing. Its step summary reports the
  numbers below.
- **Weekly**: qa.yml also runs on a schedule against `main`.
- **Daily**: `spec-drift.yml` compares the schema published at
  <https://ag-ui.com/spec/1.0/schema.json> with the pinned one
  (`cargo xtask spec-published`), so upstream republishing 1.0 is noticed.
- Actions: <https://github.com/MattJackson/agui-sdk/actions/workflows/qa.yml>

## Current result

<!-- Measured locally (corpus_values, corpus_proto_bytes, proto_corpus_bytes and
     `agui-trace gate --no-pending`); keep current on each release. -->

| Measure | Value |
| --- | --- |
| Spec version | AG-UI 1.0 |
| Upstream commit | see `spec.lock` |
| Normative requirements in the ledger | 536 rows in `docs/compliance/compliance.toml` (516 scoped to 1.0; 20 are draft-1.1 and not gated) |
| … covered by at least one test | 494 / 494 gated rows (`--no-pending` green; 599 of 600 row halves by tagged test, 1 by docs only) |
| … not applicable to an SDK (with reasons) | 22 (16 informative, 6 application-level), each with a `reason` |
| Schema fixtures (valid / invalid) passing | 135 / 84 (219 / 219 value documents) |
| Conformance streams passing | 68 / 68 target, with exactly one override (D3, below); the stream lane was being finished when this was written |
| Protobuf byte fixtures passing | 98 / 98 |
| Known divergences | 1 (D3, see below) |

## Divergences

A divergence is a place where this crate knowingly behaves differently from
the specification, or from the reference SDKs where the specification is
silent. Every one is listed here with the spec clause, what this crate does,
and why. Anything not listed is a bug.

| ID | Spec clause | Behaviour here | Reason |
| --- | --- | --- | --- |
| D3 | PROC-17 | An unknown value of a closed string set is stripped with a warning and is never fatal; in a required position its container is dropped. The Rust lane overrides the upstream fixture `unknown-enum-value-role-fatal` (`tests/corpus/rust-lane-overrides.toml`). | The spec wins: that fixture asserts the TypeScript reference's admitted gap, which its own description says contradicts the specification. |

## What the claim does not cover

- **Agent or model behaviour.** Conformance is about the protocol: what is
  sent, in what order, with what shape. Whether an agent's answers are good is
  not in scope.
- **Upstream changes after the pin.** The claim is against the pinned commit.
  Clarifications after it are picked up by bumping the pin.
- **Draft protocol features.** Only AG-UI 1.0 as published. The upstream
  `spec/draft` is not tested.
- **Interop with every other implementation.** Fixtures prove agreement with
  the specification. Agreement with any particular SDK follows from that only
  insofar as that SDK is itself conformant.
