# Security policy

`agui-sdk` parses and emits AG-UI protocol traffic: JSON events, Server-Sent
Event streams and protobuf frames exchanged between AI agents and user-facing
applications. Whichever side you run it on, **the other side's bytes are
untrusted input**, and the realistic security defects are the ones that let a
peer crash, stall, exhaust or mislead the process parsing them.

## Reporting a vulnerability

**Do not open a public issue for a security problem.**

Use **GitHub private vulnerability reporting**:
<https://github.com/MattJackson/agui-sdk/security/advisories/new> (the
Security tab, "Report a vulnerability"). It creates a private advisory only the
maintainer can see, and the fix, the advisory and the credit all happen in one
place. If that route is unavailable to you, email <matthew@pq.io>.

Please include, as far as you can:

- the crate (`agui-sdk` or `ag-ui-sdk`), version or commit, and enabled features;
- the exact input that triggers it: the SSE text, the protobuf bytes as hex, or
  the JSON events;
- what you expected and what happened, and the spec requirement involved if
  there is one;
- for a panic or hang, the API that was called.

A failing test is the most useful thing you can send.

## What to expect

- **Acknowledgement within 14 days.** If you hear nothing in that time, assume
  the message went astray and try the other channel.
- An assessment of whether the report is confirmed and what a user would have
  to be doing for it to matter.
- Updates while a fix is developed, and a disclosure date agreed with you.
  You will not be asked to wait indefinitely.

## Credit

Reporters are credited by name in the GitHub Security Advisory and in the
`CHANGELOG.md` entry for the fixing release, unless they ask not to be. Say if
you prefer a handle or a company name.

## Response process

1. **Triage**: confirmed by reproduction (a test that fails).
2. **Fix, red first**: the fix starts from that failing test.
3. **Gate**: the fix goes through the full qa gate, including fuzzing and the
   perf gate. A security fix does not skip it; the gate is much of the reason
   to believe the fix is right.
4. **Release**: a new version on crates.io for both crates, cut by tag.
5. **Advisory**: a GitHub Security Advisory, with a CVE requested where
   warranted. RustSec picks it up from there.
6. **Changelog**: the fixing version's section names the issue and links the
   advisory. The release workflow refuses a version with no section.
7. **Yank if warranted**: affected versions may be yanked. A yank never deletes
   a version; existing lockfiles keep working.

## Scope

### In scope

- **A panic reachable from peer input** through any API not documented as
  panicking: a malformed event, a truncated SSE stream, a hostile protobuf
  frame, a JSON Patch delta that does not apply.
- **Unbounded resource use driven by peer input**: an allocation, buffer or
  loop that grows without limit, or work that grows super-linearly with input
  size (the perf gate's linearity checks exist to catch this class).
- **Accepting what the spec says to reject, or emitting what it forbids**, in a
  way with a security consequence: lifecycle violations accepted silently, state
  applied from a delta that should have been refused, events attributed to the
  wrong run or message.
- **Credential or header leakage** by the client transport (for example
  forwarding authorization headers across a redirect to another origin).
- `unsafe` code anywhere in a library crate (there should be none: it is
  `#![forbid]`den).

### Out of scope

- **The AG-UI protocol's design.** Report those upstream at
  <https://github.com/ag-ui-protocol/ag-ui>. This crate is an independent
  implementation, not affiliated with the AG-UI project.
- **What an agent or model says.** The crate transports messages; whether their
  content is safe to display or execute is the application's decision.
- **Vulnerabilities in a dependency** that this crate does not expose. Report
  those to the dependency; `cargo deny` in CI fails on published RustSec
  advisories against the tree, and a fix here is a version bump.
- **TLS itself.** Certificate validation and encryption are delegated to the
  HTTP stack the `client` feature uses.

## Supported versions

Pre-1.0, **fixes land on the latest 0.9.x only**; nothing is backported. After
1.0 this section will say which lines receive fixes.

## Our own practice

- `#![forbid(unsafe_code)]` in every library crate, checked in CI.
- Every parser of peer input is fuzzed (`cargo-fuzz`), with a smoke run of
  every target in every qa gate.
- A perf gate holds the parsers to linear time and an allocation budget
  ([`docs/PERFORMANCE.md`](docs/PERFORMANCE.md)).
- `cargo deny` on every push: RustSec advisories, licences, sources.
- Conformance against the upstream specification on every qa run
  ([`docs/CONFORMANCE.md`](docs/CONFORMANCE.md)).
- Releases from CI only, via crates.io Trusted Publishing, with SLSA provenance
  and keyless cosign signatures on the release artifacts.
- The argument that this adds up to "adequately secure", and what it does not
  cover, is [`docs/ASSURANCE_CASE.md`](docs/ASSURANCE_CASE.md).
