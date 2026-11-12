# Roadmap

Direction of travel, not a schedule. Nothing here has a date. This is a
single-maintainer project ([`GOVERNANCE.md`](GOVERNANCE.md)) and the standing
priority is conformance and safety over feature count.

The protocol's own roadmap is the AG-UI project's, not this one. This crate
tracks the published specification; it does not add to it.

## 0.9.0: the complete 1.0 surface

The first release implements the whole of AG-UI 1.0, with the conformance claim
proven in CI rather than promised:

- every 1.0 event, message and input type, round-tripping against the upstream
  JSON Schema and fixtures;
- a client and a server that enforce the run lifecycle, sequential runs, and
  state snapshot / JSON Patch delta semantics;
- SSE and protobuf transports;
- every normative requirement mapped to tests in `docs/SPEC-MAP.md`, with the
  gate failing on any requirement left uncovered;
- fuzz targets over every parser, and the perf gate (allocation budgets and
  linearity) over every hot path;
- the `ag-ui-sdk` alias published alongside.

## 0.9.x: earning 1.0

0.9.x releases stay semver-compatible with each other (`cargo-semver-checks`
enforces it). What happens on this line:

- **Real-world exposure.** Interop against the reference implementations and
  against real agents and frontends. Every report of "could not talk to X" is
  a test before it is a fix.
- **Upstream tracking.** Clarifications and errata to the 1.0 specification
  are picked up by spec-sync as they land and fixed in patch releases.
- **Hardening.** Longer fuzz campaigns than the CI smoke run; mutation testing
  of the conformance-critical code; closing any gap in the
  [OpenSSF Best Practices](docs/openssf-best-practices.md) answers.
- **Performance.** Hot-spot work guided by the flamegraph and PGO jobs
  ([`docs/PERFORMANCE.md`](docs/PERFORMANCE.md)), within the budgets.
- **Ergonomics**, additively: anything that would break an API goes on the
  1.0 list instead.

## 1.0

**1.0 is a maintainer decision.** It is made when the API has carried real
agents and frontends through a stretch of 0.9.x without needing to change shape,
and when the list of deferred breaking changes is either empty or worth making
in one go. 1.0 is a promise about stability; only use earns it.

After 1.0: breaking changes need a major version, the MSRV policy in the README
continues, and a new AG-UI protocol version is supported additively where the
protocol's own compatibility rules allow.

## Further out

- **A second maintainer** with crates.io ownership and repository admin. The
  bus factor is one ([`GOVERNANCE.md`](GOVERNANCE.md#access-and-continuity)),
  and this is the single change that would most improve the project's
  resilience.
- **A CodeQL (Rust) job** in the qa gate, as a security-oriented static
  analyser alongside clippy.

## How to influence this list

Open an issue saying what you are building and what the crate made you work
around. Concrete reports from real integrations move items faster than
anything else.
