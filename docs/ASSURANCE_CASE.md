# Assurance case

An assurance case is a structured argument, supported by evidence, that
software is adequately secure for what it does. This is a short and honest one
for `agui-sdk`. It is not a proof and does not claim the crate is free of
defects; it sets out what is claimed, why, and on what evidence, so a reader
can check the argument.

Read it with [`SECURITY.md`](../SECURITY.md), which defines scope,
[`CONFORMANCE.md`](CONFORMANCE.md) and [`PERFORMANCE.md`](PERFORMANCE.md),
which hold most of the evidence.

## What the software is, for this argument

A Rust library implementing the AG-UI 1.0 agent↔UI protocol: typed events and
their JSON serialisation, a client that consumes an agent's event stream, a
server side that produces one, and two wires (SSE and protobuf). The client
talks HTTP/1.1 through `hyper` (with `hyper-util`); TLS is delegated to rustls or the platform stack.
The crate holds no keys, performs no cryptography of its own, stores nothing,
and executes nothing it receives.

## Claim

> **A peer cannot use `agui-sdk` to crash, stall or exhaust the process using
> it, or to make it misreport the conversation.** For any bytes a peer sends,
> the crate either produces the events, state and messages the AG-UI 1.0
> specification defines, or reports an error or diagnostic. It does not panic,
> does not use unbounded memory or super-linear time, and does not emit
> anything the specification forbids.

## Threats considered

| # | Threat | Why it matters |
|---|---|---|
| T1 | **Malformed or hostile input causes a panic** (truncated SSE, invalid JSON, a crafted protobuf frame) | A panic in a streaming handler takes down a request, or a process. |
| T2 | **Resource exhaustion**: an unbounded buffer, a huge field, deep nesting, or an accidentally quadratic path | The classic DoS against parsers; invisible to small-input tests. |
| T3 | **State corruption**: a `STATE_DELTA` JSON Patch or a `MESSAGES_SNAPSHOT` that leaves state inconsistent, or that is applied when the spec says to refuse it | The UI shows a conversation that did not happen. |
| T4 | **Lifecycle confusion**: events attributed to the wrong run, message or tool call; a second run accepted mid-run | Same harm as T3, through ordering. |
| T5 | **Spec drift**: the implementation silently diverges from the specification | Interop failures that look like the peer's fault. |
| T6 | **Memory-safety defects** | The classic class. |
| T7 | **Supply chain**: a compromised or vulnerable dependency, action, or release | The crate is only as safe as what it builds from and how it ships. |
| T8 | **Transport leaks**: credentials sent somewhere they should not be | The client carries the application's auth headers. |

## Sub-claims and evidence

### 1. Memory safety by construction (T6)

*Argument.* Every library crate forbids `unsafe` (`forbid`, not `deny`, so no
module can opt back in). Remaining `unsafe` exists only inside well-reviewed
dependencies (`serde_json`, `bytes`, `tokio`, `hyper`, rustls).

*Evidence.* `#![forbid(unsafe_code)]` / `[lints.rust] unsafe_code = "forbid"`;
qa.yml's lint job fails if either published crate stops forbidding it; the
README badge is a claim the project keeps true.

### 2. No panic on peer input (T1)

*Argument.* Every parser returns `Result`; peer input never reaches `unwrap`,
indexing or arithmetic that can panic. Fuzzing varies the input rather than
choosing it.

*Evidence.* `cargo-fuzz` targets over every parser of peer input, a smoke run of
every target in every qa gate (qa.yml `fuzz`), with crash reproducers kept as
artifacts and turned into regression tests. Clippy with `-D warnings` on every
push.
The nine fuzz targets in `fuzz/fuzz_targets/` are `admit_input`,
`enforce_event`, `expiry_parse`, `patch_apply`, `pipeline_json`,
`proto_decode`, `proto_frames`, `sse_decoder` and `version_parse` (`common.rs`
is shared helpers). qa runs each for 60 seconds per gate (`FUZZ_SECONDS` in
qa.yml). No longer campaign has been run; none is recorded here, so none is
claimed.

### 3. Bounded resources, linear cost (T2)

*Argument.* Buffers that grow with peer input have limits; exceeding one is an
error. Cost per event is budgeted and the growth of cost with input size is
measured, not assumed.

*Evidence.* The perf gate (perf.yml, run from qa.yml): exact allocation-count
budgets and linearity checks on every hot path
([`PERFORMANCE.md`](PERFORMANCE.md)). Bounded JSON Patch application in the
default build. The client's default `Limits` keep what one stream makes it
hold near 252 MiB: about 124 MiB of patched state and activity content plus
128 MiB of history and id bookkeeping growth, charged in estimated resident bytes
([`DECISIONS.md`](DECISIONS.md) D14), checked against a counting allocator
by `tests/history_resident.rs`.

### 4. State and lifecycle follow the specification (T3, T4)

*Argument.* The run lifecycle, sequential runs, and the snapshot/delta rules
are implemented as a verifier the client applies to incoming streams and the
server applies to what it emits, so a violation is caught on whichever side
it originates.

*Evidence.* Requirement-traced tests (`/// covers: ID`, ledger in
[`SPEC-MAP.md`](SPEC-MAP.md)); the upstream conformance streams in the
spec-sync job ([`CONFORMANCE.md`](CONFORMANCE.md)).

### 5. Conformance is re-checked against upstream, not remembered (T5)

*Argument.* The tests run against the specification's own schema and fixtures,
fetched at a pinned commit and hash-verified, so the crate cannot drift from a
private copy. Every normative requirement has a test, enforced by a
traceability check.

*Evidence.* qa.yml `spec-sync`, on every qa run, every release and weekly,
against the pinned commit; and `spec-drift.yml`, daily, which fails when the
schema published at the spec's `schema_url` stops matching the pin (a weekly
qa run alone re-reads the pinned bytes and cannot see upstream change).

### 6. Supply chain (T7)

*Argument.* Minimal dependencies (the default build is `serde` + `serde_json`),
each justified in writing; a committed lockfile used with `--locked`;
continuous advisory checks; pinned CI; releases only from CI, with signatures.

*Evidence.* `deny.toml` and `cargo deny check` on every push and weekly;
Dependabot for cargo and actions; every action pinned by full SHA; explicit
least-privilege `permissions`; crates.io Trusted Publishing from a protected
`release` environment; SLSA provenance and cosign signatures on release
artifacts; OpenSSF Scorecard weekly.

### 7. Transport hygiene (T8)

*Argument.* TLS is not implemented here; it is delegated to rustls or the
platform stack, with certificate validation on (custom roots and client
identities only add trust material; there is no "accept invalid" switch).
Redirects are handled by the transport itself (hyper follows none): only
same-origin redirects are followed, up to a configured count (default 0),
and cross-origin redirects are never followed, so credentials never reach
another origin. The environment proxy is read only when enabled.

*Evidence.* `tests/transport_http.rs`: `redirects_are_not_followed_by_default`,
`cross_origin_redirects_are_never_followed`, `method_changing_redirects_and_loops_stop`
(redirect behaviour), and `sends_transport_and_per_run_headers`,
`reserved_headers_cannot_be_overridden`, `url_credentials_become_basic_auth`
(headers). The redirect policy is implemented in
`src/client/transport/http/exchange.rs`.

### 8. Every claim is traceable (all)

Requirements trace to tests through `covers:` tags; divergences are listed with
reasons in [`CONFORMANCE.md`](CONFORMANCE.md); performance budgets are written
down in [`PERFORMANCE.md`](PERFORMANCE.md). A claim with no evidence named here
should be treated as unproven.

## Residual risks, stated plainly

- **Dependencies.** A defect in `serde_json`, `hyper`, `hyper-util`, `tokio`,
  rustls or `axum` is outside this crate's control. Mitigated by advisories in
  CI and prompt updates, not eliminated.
- **The specification itself.** If the spec permits something harmful, a
  conformant implementation permits it too.
- **Fuzzing is a smoke run in CI.** One minute per target catches regressions,
  not deep bugs. Longer campaigns are run by hand and are on the roadmap.
- **Application misuse.** Displaying model output as HTML, executing tool-call
  arguments, or trusting `RAW`/`CUSTOM` event payloads is the application's
  decision.
- **Single maintainer.** Review is by one person ([`GOVERNANCE.md`](../GOVERNANCE.md));
  the automated gates are what make that tolerable.

## Conclusion

The memory-safety class is removed by construction; the parser-DoS class is
attacked from two sides (fuzzing for panics, the perf gate for cost); and
correctness against the specification is checked continuously against the
specification's own fixtures. What remains is named above. The argument holds
only as long as the gates it cites keep running, which is why each one is a
required check rather than a report.
