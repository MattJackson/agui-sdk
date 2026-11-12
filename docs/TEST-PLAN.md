# agui-sdk test plan

What proves the SDK conforms, where each test lives, how upstream spec data
reaches the tests without being vendored, and how performance is held to the
budgets in `API.md` §8.

Working rule: locally, run fmt, clippy and the few tests that cover the
change. CI runs the full suites, the corpus lanes, the feature matrix, MSRV,
fuzz smoke runs and the perf gates.

## 1. Layout

```
tests/
  <area>_<topic>.rs            client-side rule tests (one file per spec area/topic)
  <area>_producer.rs           server-side rule tests (producer verifier, runner, emitters)
  transport_*_server.rs        server transport tests
  corpus_streams.rs            stream lane (upstream conformance/streams)
  corpus_values.rs             value lane (upstream fixtures/)
  corpus_proto_bytes.rs        protobuf byte lane (upstream TS byte corpus)
  producer_fixtures.rs         producer lane (our tests/fixtures/producer/)
  spec_drift.rs                hand-written types vs schema.json; generated tables up to date
  traceability.rs              the strict gate (also `cargo xtask trace`)
  fixtures/producer/*.json     our producer fixtures
  corpus/rust-lane-overrides.toml   the only allowed corpus override (DECISIONS D3), each with a reason
benches/                       harness = false; counting allocator; see §9
fuzz/                          cargo-fuzz targets; see §8
```

The 45 rule files, and which rows each must cover, are fixed by `SPEC-MAP.md`:

| Area | Client-side files | Server-side files |
|---|---|---|
| lifecycle | `lifecycle_runs.rs`, `lifecycle_usage.rs` | `lifecycle_producer.rs`, `lifecycle_usage.rs` |
| streaming | `streaming_triads.rs`, `streaming_chunks.rs` | `streaming_producer.rs` |
| text | `text_messages.rs` | `text_producer.rs` |
| tools | `tools_calls.rs`, `tools_frontend.rs`, `tools_safety.rs` | `tools_producer.rs` |
| state | `state_snapshots.rs`, `state_messages_snapshot.rs` | `state_producer.rs` |
| activity | `activity_messages.rs` | `activity_producer.rs` |
| reasoning | `reasoning_spans.rs`, `reasoning_encrypted.rs` | `reasoning_producer.rs` |
| subagents | `subagents_attribution.rs` | `subagents_producer.rs` |
| passthrough | `passthrough_events.rs` | `passthrough_events.rs` |
| metadata | `metadata_merge.rs` | `metadata_builder.rs` |
| processing | `processing_enforce.rs`, `processing_order.rs`, `processing_input.rs` | `processing_producer.rs` |
| versioning | `versioning_versions.rs`, `versioning_era.rs` | `versioning_producer.rs` |
| run input | `run_input_client.rs` | `run_input_server.rs` |
| interrupts | `interrupts_resume.rs` | `interrupts_producer.rs` |
| capabilities | `capabilities_ingest.rs` | `capabilities_builder.rs` |
| transports | `transport_contract.rs`, `transport_sse.rs`, `transport_proto.rs` | `transport_contract.rs`, `transport_sse_server.rs`, `transport_proto_server.rs` |
| event model, security | `basic_events.rs`, `security_consent.rs` | `basic_events.rs`, `security_consent.rs` |

Module-private behaviour (SSE line assembly, patch undo log, lane table) gets
unit tests in `#[cfg(test)] mod tests` next to the code. Rule tests go
through public API only.

Client rule tests drive `pipeline::Processor` directly when the rule is about
verdicts. They drive `AgentClient` over `agui_test_support::mock::MockTransport`
when the rule is about the client surface (statuses, errors, partial reports,
observers). Server rule tests drive `Runner` with small test agents, and
`ProducerVerifier` directly for must-refuse cases.

## 2. Upstream spec data: fetched, pinned, never vendored

- **`spec.lock`** (repository root) pins the upstream repository, the
  `commit` (currently `deefd3833c15b353348ba90eae0fad6113021759`), the
  archive `sha256`, and the `paths` the tests read:
  - `spec/1.0/schema.json` and `spec/1.0/proto-freeze.txt`;
  - `spec/1.0/conformance/streams`;
  - `spec/1.0/fixtures`;
  - the protobuf `.proto` sources and the TS byte corpus
    (`sdks/typescript/packages/proto/__tests__/__fixtures__/bytes`).
- **`cargo xtask spec-fetch`** downloads
  `https://codeload.github.com/ag-ui-protocol/ag-ui/tar.gz/<commit>` with
  `curl`, verifies `sha256`, and extracts only `paths` into
  `.spec-cache/<commit>/` (gitignored). It is idempotent: an existing cache
  for the pinned commit is reused.
- **`cargo xtask spec-bump <sha>`** is the only way the pin moves. It
  rewrites `spec.lock`, re-fetches, runs `codegen` and `drift`, and the
  change goes in its own PR with the corpus diff in the description.
- **Location at test time:** `$AGUI_SPEC_DIR` if set, else
  `<workspace>/.spec-cache/<commit>`. `agui_test_support::spec::root()`
  checks that a `.commit` stamp in the cache matches `spec.lock`.
- **Missing cache.** Each corpus test prints `spec cache missing for
  <commit>: run 'cargo xtask spec-fetch'` and skips; nothing passes silently.
  In CI, `AGUI_REQUIRE_SPEC=1` turns the skip into a failure, so the gate
  cannot be green without the data. A stale cache (wrong commit) is always a
  failure.
- **Discovery, not counts.** Corpora are enumerated from their upstream
  `MANIFEST.txt` and cross-checked against the directory in both directions.
  Counts are printed, never asserted. Today they are 68 stream fixtures,
  219 value documents (135 valid, 84 invalid) and 98 protobuf byte cases.

## 3. Corpus lanes

### Stream lane: 68 fixtures (`tests/corpus_streams.rs`)

Each fixture runs four ways, and every way must satisfy the fixture's
`expect` block (outcome, delivered events, warnings, final messages and
state):

1. **frames**: the events straight into `Processor::push_json`;
2. **SSE**: encoded as SSE text, delivered through `MockTransport` split
   every 1, 7 and 4096 bytes, decoded by `SseDecoder`;
3. **protobuf**: encoded with `proto::encode::encode_json`, framed, split
   every 1 and 4096 bytes, decoded. This runs only for fixtures whose defect
   the binary wire can express, and the skip list is computed (an event that
   protobuf cannot carry), never hand-written. This lane is what makes PB-14
   genuinely fixtured;
4. **middleware**: with an identity middleware that forces the `Frame` →
   `Value` path, so `push_value` is held to the same verdicts.

The oracle (`agui_test_support::oracle`) **fails closed**: an expectation key
it does not understand fails the fixture, and supporting a new key is an
explicit code change. Error-text expectations (`errorContains`) are met by
the SDK's own messages, which reuse the corpus-pinned wording where a fixture
pins it. The single permitted override is DECISIONS D3 (closed-set unknown value),
recorded in `tests/corpus/rust-lane-overrides.toml` with its reason.

### Value lane: 219 documents (`tests/corpus_values.rs`)

- **Gate A, strict.** `pipeline::enforce::validate_strict(anchor, bytes)`
  accepts all 135 valid documents and rejects all 84 invalid ones.
- **Gate B, consumer pipeline.**
  `pipeline::enforce::enforce_document(anchor, value)` accepts every valid
  document unchanged and with no diagnostics. It accepts the invalid documents
  whose only defect is unknown material (unknown property, enum value or union
  member: 10 today), stripping it with the expected diagnostic, and rejects
  every other invalid document. Which invalid documents are "unknown material
  only" is computed from the schema, not listed by hand.
- **Round trip.** Every valid event document parses into `Event`,
  re-serializes, and is semantically equal (`serde_json::Value` equality) to
  the input. Optional fields are omitted, never `null`.

### Protobuf lane: 98 byte cases (`tests/corpus_proto_bytes.rs`)

- `encode_json(json)` is **byte-identical** to the `.bin` (PB-12).
- `decode_event(bin)` yields a value semantically equal to the JSON, with
  the documented empty-materialisation exception (PROC-09).
- Typed path: JSON → `Event` → `encode_event` → `decode_event` is
  semantically equal (not byte-equal for open objects; see SPEC-MAP flags).
- Framing: every case framed, concatenated, and re-split at every byte
  offset for the first 16 cases (PB-09).

## 4. Producer fixtures (`tests/producer_fixtures.rs`)

Consumer fixtures are never replayed through the producer verifier: at
least 12 "completed" consumer fixtures contain material a producer must never
emit. The producer lane uses this repository's own fixtures in
`tests/fixtures/producer/` instead:

```json
{ "description": "...", "covers": ["SUB-08"],
  "events": [ { "type": "RUN_STARTED", ... }, ... ],
  "verdict": "ok" }
{ ..., "verdict": { "refuse": "SubagentIdentity", "at": 3 } }
```

- **Producer-legal streams** must pass `ProducerVerifier` end to end, and
  must also pass the consumer pipeline (a producer-legal stream is always
  consumer-legal).
- **Must-refuse cases**: one per producer MUST NOT. The verifier must refuse
  exactly the event at `at`, with that rule, and must leave its state
  unchanged (the next legal event is still accepted).
- The 69 producer fixtures from the earlier prototype are original work and
  are converted to this format.

## 5. Traceability and the strict gate

- **Tag format.** A doc line directly above the test function:

  ```rust
  /// covers: LIFE-41, STRM-12
  #[test]
  fn late_run_error_after_finish_is_accepted() { ... }
  ```

  The format is one `covers:` line per test, comma-separated IDs, IDs
  exactly as in `SPEC-MAP.md`. Corpus-lane tests may tag by fixture
  through the fixtures column instead; the gate reads both.
- **Gate** (`cargo xtask trace`, also run as `tests/traceability.rs`):
  1. every gated row in `SPEC-MAP.md` (sdk-ownable and sdk-assistable,
     scope 1.0) has at least one tagged test in each file the map names;
  2. every tagged ID exists in the map;
  3. a row tagged only in files other than its mapped one fails: move the
     test or change the map in the same PR;
  4. assistable rows need a test of their helper, not just docs (TRN-14 is
     the one docs-only row: its test is a doc-test asserting the documented
     sections exist).
- **Strict.** There is no allow-list and no "pending" state. The gate is red
  until every row is covered. It is a required CI check.

## 6. Drift (`tests/spec_drift.rs`, `cargo xtask drift`)

- The generated enforcement shape table (`src/pipeline/shapes.rs`) and the
  protobuf field table (`src/proto/fields.rs`) must equal what `cargo xtask
  codegen` produces from the pinned spec.
- The hand-written protocol types must have exactly the schema's field sets.
  For every `$defs` object, the struct's serde field names, which are
  required and which optional, and the enum value sets are compared with the
  schema. A new upstream field fails the drift test on the next `spec-bump`.

## 7. Other test kinds

- **Property tests** (`proptest`, dev-dependency):
  - chunk expansion: any interleaving of chunk and explicit events either
    expands to a stream the verifier accepts, or fails with a `verify.chunk`
    code;
  - patch: `apply` either succeeds or leaves the document bit-identical;
    the budget's node count equals a full recount;
  - `JsonObject::merge_from` is last-write-wins and shallow;
  - SSE and protobuf decoding are invariant under re-chunking.
- **Snapshot tests** for error and diagnostic messages, checked as plain
  expected strings in the test (no snapshot crate).
- **Compile-fail doc-tests** for the no-bypass guarantees: no public path
  yields events without the pipeline (PROC-31), and `Emitter::emit` refuses
  `RUN_*` (by test, since it is a runtime refusal).
- **Interop:** `tests/interop_ts.rs`, gated by `AGUI_INTEROP=1`, with its
  Node half in `interop/ts/` (pinned `@ag-ui/client`, `@ag-ui/core` and
  `@ag-ui/encoder` from npm, lockfile committed, `node_modules` ignored;
  neither is packaged). The TS `HttpAgent`, with its enforcement and
  `verifyEvents`, runs against this crate's server through the axum glue
  and must accept every event; this crate's `AgentClient` runs against a run
  the TS encoder serves, over SSE and over negotiated protobuf. qa.yml's
  `ts-interop` job runs it on every qa run and release.
- **Packaging:** `cargo package --list` must not contain `.spec-cache/`,
  `fuzz/`, `target/` or `interop/`, and `cargo publish --dry-run --workspace`
  builds both crates from their packages (default features).
- **Semver:** `cargo semver-checks` against the last release.
- **Supply chain:** `cargo deny check` (licences, advisories, sources) on the
  latest lock.

## 8. Fuzzing (`fuzz/`, cargo-fuzz)

| Target | Entry | Invariants |
|---|---|---|
| `sse_decoder` | `SseDecoder::push`/`next_event` over arbitrary chunking | no panic; memory bounded by the limit; same events regardless of chunking |
| `proto_frames` | `FrameDecoder` | no panic; oversize rejected before buffering |
| `proto_decode` | `decode_event` | no panic; `Ok` output re-encodes to an equivalent value, except an `int64` outside ±(2^53−1), which the decoder hands on exactly and the encoder must refuse with `Encode` |
| `pipeline_json` | `Processor::push_json` over an arbitrary event sequence | no panic; every `Ok` event is post-enforcement (strict-validates) |
| `patch_apply` | `patch::apply` on arbitrary document and ops | no panic; atomicity; budget never exceeded |
| `admit_input` | `server::input::admit` | no panic; `Ok` input strict-validates |
| `expiry_parse` | `Interrupt::expiry` | no panic; total order on valid timestamps |
| `version_parse` | `ProtocolVersion::parse`, `LegacyEra::parse` | no panic |
| `enforce_event` | `Enforcer::enforce_event_json` over arbitrary bytes | no panic; every accepted event strict-validates |

qa's `fuzz` job runs every target `cargo fuzz list` finds, for 60 s each, on
every qa run (nothing runs per PR or nightly). The corpora are seeded from the
pinned spec on each run (`agui-fuzz-seed`), not cached, and crashes become
regression unit tests.

## 9. Performance plan

Performance budgets are in `API.md` §8. They are enforced here.

### 9.1 Benches (`benches/`, `harness = false`, no criterion)

A small in-repo harness (`benches/support/mod.rs`): a warm-up, then N timed
iterations with `std::time::Instant`. It reports median and p95 ns/event,
MB/s, and **allocations per event**, using a counting global allocator
installed in each bench binary (wrapping `std::alloc::System`, counting
allocations, reallocations and bytes). The counter lives in benches only, so
it never affects the library.

| Bench | Measures | Budget asserted |
|---|---|---|
| `sse_decode` | 1 MB stream of typical events, chunked 1 KiB / 64 KiB | 0 allocs/event steady state |
| `proto_decode` | framing + decode, same stream | 0 allocs/event for framing; decode ≤ owned fields |
| `pipeline_push_json` | enforce + expand + verify + reduce, text-heavy stream | allocs/event ≤ owned strings per event (counted from fixture) |
| `pipeline_push_value` | same through the middleware path | as above + 0 extra |
| `reducer_deltas` | 100k `TEXT_MESSAGE_CONTENT` into one message, updates dropped (`one_message`) or held (`one_message_held`) | O(1) amortized: allocs ≤ log2(total bytes) when dropped (the `Arc` is recycled); + one `Arc<Event>` per event when held |
| `messages_snapshot` | reconcile 10k messages | O(n) time |
| `patch_apply` | 1k-op patches on 1 MB state; failing patches | allocated bytes ≤ 8 × ops' size + 4 KiB (no document clone); exact allocation count in `budgets.toml` |
| `server_emit_encode` | producer verifier + SSE / proto encode over pre-built events (the runner is `server_runner`) | 0 allocs/event in the encoder |

### 9.2 Linearity gates

`cargo bench` runs each gated case at input sizes ×1 and ×8 (CI's quick
mode); `AGUI_BENCH_MODE=full` adds ×2 and ×4. The gate fails when the ×8/×1
ratio exceeds 8 × 1.15 for allocations or 8 × 2.0 for time (`[gate]` in
`benches/budgets.toml`; the per-case time exceptions for cases that outgrow
the CPU caches are in docs/PERFORMANCE.md), or when the per-event allocation
count differs at all from its budget in `benches/budgets.toml`. The
allocation count is deterministic, so any change is either a bug or an
intentional budget edit in the same PR. All of these are hard failures,
time linearity included.

### 9.3 Profiling each hot path

For every bench built on `benches/support` (the ones that call
`Bench::finish`):

1. `cargo bench --bench <name> --no-run` and note the binary path;
2. Linux: `cargo flamegraph --bench <name> -- --profile-time 10`, or
   `perf record -g` on the bench binary;
3. macOS and Linux: `samply record target/release/deps/<name>-*
   --profile-time 10` and inspect in the Firefox Profiler;
4. record the top three frames per bench in the PR when a budget changes.

The self-harnessed benches (`pipeline_linearity`, `protocol_types`,
`proto_codec`, `server_runner`, `http_transport`) do not take
`--profile-time`: profile them by running the bench binary with `--bench`
under `perf record -g` or `samply record` directly.

`[profile.bench]` keeps `debug = "line-tables-only"` so the stacks are
readable.

### 9.4 PGO workflow (hot-spot discovery, and for consumers)

Used by maintainers to find hot spots the sampling profiler blurs, and
documented for applications that build binaries with this crate:

1. `rustup component add llvm-tools-preview`; install `cargo-pgo`;
2. `cargo pgo build`, then run a representative workload: the benches,
   or the application replaying recorded agent streams;
3. `cargo pgo optimize`, and compare the bench numbers;
4. maintainers diff `llvm-profdata show --topn=50` between commits to spot
   newly hot functions;
5. consumers: the same steps on their own binary. The crate needs no special
   flags, and the protocol hot paths respond well to PGO because the event
   type distribution is skewed (content deltas dominate).

### 9.5 CI perf jobs

All in `perf.yml`:

- **gate** (called by qa.yml, so on every qa run, weekly and on every
  release; also dispatchable): `cargo bench` in quick mode (×1 and ×8
  sizes). Allocation budgets (exact, `benches/budgets.toml`) and linearity
  are hard failures. No throughput floor is set yet: every case has
  `min_mb_per_s = 0`, and an unset floor only prints (docs/PERFORMANCE.md). The measurements are
  uploaded as an artifact. Nothing runs per PR or nightly.
- **flamegraph** and **pgo**: `workflow_dispatch` only (input `profile`), a
  flamegraph SVG per bench target and a PGO before/after delta, as
  artifacts.

## 10. CI matrix (summary)

What the workflows run, by file and job. `dev.yml` is the inner loop on
every push to dev; `qa.yml` is the thorough gate (pushes to qa, weekly, and
called by `release.yml` on the tag before publishing); its `gate` job fails
on any upstream job that did not succeed. Third-party actions are pinned to
commit SHAs, and installed tools to exact versions.

| Workflow / job | What |
|---|---|
| dev `check` (Linux, stable) | `cargo fmt --check`; `cargo clippy -p agui-sdk --lib --no-default-features -- -D warnings` (the library alone, no features); `cargo clippy --workspace --all-targets -- -D warnings`, plain (the test harness turns on `client`, `server` and `proto`, so effectively those three) and `--all-features`; `cargo test --workspace --all-targets`, plain (`client`, `server`, `proto` through the harness) and `--all-features`; doctests |
| dev `deny`, qa `deny` | `cargo deny --all-features check` |
| qa `manifest` | version and MSRV read from Cargo.toml; README badge and clippy.toml agree |
| qa `lint` (Linux, stable) | fmt; clippy `--workspace --all-targets`, plain (the test harness turns on `client`, `server` and `proto`, so effectively those three) and `--all-features`; every Cargo.toml feature has an `msrv-features` row; the `unsafe: forbidden` claim; the release.yml guard (every cargo line there is `publish`/`package --no-verify`, D16) |
| qa `test` (Linux, macOS, Windows) | `cargo xtask spec-fetch`; `AGUI_REQUIRE_SPEC=1 cargo test --workspace --all-targets`, plain (`client`, `server`, `proto` through the harness) and `--all-features`; doctests |
| qa `msrv` (Linux, macOS, Windows) | `cargo xtask msrv-lock` (drift from the committed lock is a warning); `cargo +1.75 rustc -p agui-sdk --lib --no-default-features --profile check -- -D warnings`; `cargo +1.75 build` of both crates, default features (no test target, so truly default); `cargo +1.75 test --lib --tests` of both crates, where the harness unifies in `client`, `server` and `proto` |
| qa `msrv-all-features` (Linux) | two legs, one toolchain each: `cargo +1.75 check` and `test --locked` with every feature but native-tls, and the same with every feature on the native-tls floor 1.80; both crates, on the committed lock |
| qa `msrv-features` (Linux) | one row per feature (11) plus one with no features, committed lock: the library alone checked at the feature's floor (1.80 for `native-tls`, 1.75 otherwise) with `cargo rustc --lib --profile check --no-default-features [--features <f>] -- -D warnings`, and clippied on stable with `cargo clippy --lib --no-default-features [--features <f>] -- -D warnings`; no test target is built, so nothing is unified in (D15). Then, for each feature row, `cargo test --lib --tests --features <f>` at the floor, which runs with that feature plus the harness's `client`, `server` and `proto` |
| qa `docs` (Linux) | with `-D warnings` and the intra-doc link lints: `cargo doc --workspace --all-features --no-deps`; `cargo doc -p agui-sdk --no-deps --no-default-features`, alone and with each feature on its own |
| qa `spec-sync` | `cargo xtask spec-fetch`; `cargo xtask drift`; the corpus lanes and the workspace tests with `AGUI_REQUIRE_SPEC=1`; `agui-trace gate --no-pending` |
| qa `fuzz` | every cargo-fuzz target for 60 s, on a dated nightly, seeded from the pinned spec |
| qa `perf` | §9.5 |
| qa `ts-interop` | `npm ci` in `interop/ts`; `AGUI_INTEROP=1 cargo test --test interop_ts` (§7) |
| qa `package` | the package lists; `cargo publish --dry-run --workspace` (default features); this is the release's token-free verify build (release.yml publishes `--no-verify`) |
| qa `coverage` | `cargo llvm-cov --workspace --all-features`, line floor; Codecov upload when configured |
| qa `semver` | `cargo semver-checks --all-features` for each crate with a published baseline |
| `spec-drift.yml` (daily, dispatch) | `cargo xtask spec-published`: the published 1.0 schema must equal the pinned one |
| `release.yml` | qa.yml on the tag; publish both crates; package, check the bytes against crates.io, attest and sign; GitHub release |
| `scorecard.yml` (weekly, dispatch) | OpenSSF Scorecard |

`tests/native_tls_floor.rs` (the native-tls TLS 1.2 floor, D19) compiles only
with `native-tls` on Linux, so it runs in the Linux legs that enable every
feature: dev `check` (`--all-features`, ubuntu), qa `test`'s ubuntu leg
(`--all-features`), qa `msrv-all-features`'s 1.80 leg and qa `coverage`. It sets
`OPENSSL_CONF` to a permissive configuration, so it is one test in its own
binary.
