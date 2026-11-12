# OpenSSF Best Practices: passing-level answers for agui-sdk

A pre-filled draft of the [OpenSSF Best Practices](https://www.bestpractices.dev)
**passing** questionnaire, answered against what this repository actually
does. It is meant to be pasted, not composed: each entry gives the answer, the
justification to type into the box, and the URL evidence. Nothing here is
aspirational; where the project does not meet a criterion it says **Unmet**
and what would close it.

- Criteria IDs and order follow the form (67 passing-level criteria).
- Register as **Matthew Jackson, matthew@pq.io**, via GitHub sign-in.
- Project URL and repository URL: `https://github.com/MattJackson/agui-sdk`.
- Steps 0–1 of [`SETUP.md`](SETUP.md) must be done first: many URLs below
  point at `blob/main/...`, and two answers rely on private vulnerability
  reporting being enabled.

## How the levels are scored

- **MUST**: Met or N/A, or the badge is blocked.
- **SHOULD**: Met or N/A, or Unmet with a justification. An explained Unmet
  still passes.
- **SUGGESTED**: only has to be answered.

## Before submitting

1. **Owner to confirm**: `know_secure_design` and `know_common_errors` are
   claims about the maintainer, not the code. The evidence is assembled; the
   claim is yours.
2. **[JUDGEMENT]** marks answers with a defensible alternative; the reasoning
   is given.
3. Answers that depend on a first release (`version_tags`, `release_notes`)
   are Met only once v0.9.0 is tagged. Submit after the first release, or
   answer them Unmet and update later.

Base URL used below: `https://github.com/MattJackson/agui-sdk`, written `REPO`.

---

## Basics

### Basic project website content

**`description_good` (MUST): Met.**
The README opens by stating it is a Rust SDK for the AG-UI 1.0 agent↔UI
protocol and explains what AG-UI is and what the crate provides.
`REPO#agui-sdk`

**`interact` (MUST): Met.**
README covers obtaining it (crates.io, `[dependencies]` snippet), feedback
(issues, with templates), and contributing (CONTRIBUTING.md).
`REPO#contributing`

**`contribution` (MUST): Met.**
CONTRIBUTING.md explains the branch flow, pull requests against `dev`, and the
checks CI enforces.
`REPO/blob/main/CONTRIBUTING.md`

**`contribution_requirements` (SUGGESTED): Met.**
CONTRIBUTING.md states the coding standards (machine-checked table), testing
policy, Conventional Commits, DCO sign-off, spec-traceability tags and the
dependency-justification rule.
`REPO/blob/main/CONTRIBUTING.md#coding-standards`

### FLOSS license

**`floss_license` (MUST): Met.** MIT.
`REPO/blob/main/LICENSE`

**`floss_license_osi` (SUGGESTED): Met.** MIT is OSI-approved.
`REPO/blob/main/LICENSE`

**`license_location` (MUST): Met.**
`LICENSE` at the repository root; REUSE-compliant, with `REUSE.toml` and
`LICENSES/`.
`REPO/blob/main/LICENSE`

### Documentation

**`documentation_basics` (MUST): Met.**
README (what it is, quick start, features, MSRV, versioning), crate-level
rustdoc on docs.rs, and `docs/`.
`REPO#readme`, `https://docs.rs/agui-sdk`

**`documentation_interface` (MUST): Met.**
Every public item is documented (`missing_docs = "deny"`), published on docs.rs;
`cargo doc` with `-D warnings` and broken-link lints runs in the qa gate.
`https://docs.rs/agui-sdk`

### Other

**`sites_https` (MUST): Met.**
GitHub, crates.io and docs.rs are all HTTPS-only.

**`discussion` (MUST): Met.**
GitHub issues and pull requests: searchable, addressable by URL, open to anyone
with an account, no proprietary client needed.
`REPO/issues`

**`english` (SHOULD): Met.**
All documentation is in English, and reports are accepted in English.

**`maintained` (MUST): Met.**
Actively developed; Dependabot and weekly scheduled CI runs keep it current.
`REPO/commits/main`

---

## Change Control

### Public version-controlled source repository

**`repo_public` (MUST): Met.** Public Git repository on GitHub. `REPO`

**`repo_track` (MUST): Met.** Git records who changed what and when; commits are
signed off (DCO). `REPO/commits/main`

**`repo_interim` (MUST): Met.**
All work lands on the public `dev` branch before release, not just release
snapshots. `REPO/tree/dev`

**`repo_distributed` (SUGGESTED): Met.** Git.

### Unique version numbering

**`version_unique` (MUST): Met.**
Each release has a unique SemVer version, published to crates.io (which
refuses re-publication of a version) and tagged `vX.Y.Z`.
`https://crates.io/crates/agui-sdk/versions`

**`version_semver` (SUGGESTED): Met.**
SemVer, with `cargo-semver-checks` against the published crate in the qa gate.
`REPO#versioning`

**`version_tags` (SUGGESTED): Met after the first release.**
Releases are created *from* `vX.Y.Z` tags (release.yml triggers on them).
`REPO/tags`

### Release notes

**`release_notes` (MUST): Met after the first release.**
CHANGELOG.md (Keep a Changelog); every GitHub Release body is that version's
section, and the release workflow refuses a version without one.
`REPO/blob/main/CHANGELOG.md`

**`release_notes_vulns` (MUST): N/A.**
No publicly known vulnerabilities have been fixed. SECURITY.md commits to
naming them in the changelog and advisory when there are.

---

## Reporting

### Bug-reporting process

**`report_process` (MUST): Met.**
GitHub issues, with bug and feature templates.
`REPO/issues/new/choose`

**`report_tracker` (SHOULD): Met.** GitHub issues. `REPO/issues`

**`report_responses` (MUST): Met.**
The maintainer acknowledges bug reports. [JUDGEMENT] For a newly published
project there may be no reports in the last 2–12 months; the form accepts Met
with that stated, or answer once there is history.

**`enhancement_responses` (SHOULD): Met.** As above, for feature requests.

**`report_archive` (MUST): Met.**
Issues and pull requests are a public, searchable archive. `REPO/issues?q=`

### Vulnerability report process

**`vulnerability_report_process` (MUST): Met.**
SECURITY.md describes how to report, what to expect and the process.
`REPO/blob/main/SECURITY.md`

**`vulnerability_report_private` (MUST): Met** (once private vulnerability
reporting is enabled, SETUP.md step 1).
`REPO/security/advisories/new`

**`vulnerability_report_response` (MUST): N/A.**
No vulnerability reports in the last 6 months. SECURITY.md commits to
acknowledgement within 14 days.

---

## Quality

### Working build system

**`build` (MUST): Met.** Cargo. `cargo build`.

**`build_common_tools` (SUGGESTED): Met.** Cargo, the standard Rust build tool.

**`build_floss_tools` (SHOULD): Met.** Rust, Cargo and every CI tool used are FLOSS.

### Automated test suite

**`test` (MUST): Met.**
`cargo test`; unit, integration, doc and conformance tests, released as part of
the source. `REPO/blob/main/CONTRIBUTING.md#before-you-push`

**`test_invocation` (SHOULD): Met.** `cargo test`, the standard invocation.

**`test_most` (SUGGESTED): Met.**
[JUDGEMENT] Coverage is measured in qa (`cargo llvm-cov`) and every normative
spec requirement is traced to a test. The only figure measured so far is
line coverage of the lib unit tests alone: 38.25% (`cargo llvm-cov
--all-features --lib`); the integration, corpus and doc tests are not in it,
and the qa coverage job gates at 33% lines. The workspace-wide figure has not
been measured, so it is not stated here. If that figure is below roughly 80%,
answer Unmet until it is not.

**`test_continuous_integration` (SUGGESTED): Met.**
GitHub Actions on every push and pull request (dev.yml), full gate on qa.
`REPO/actions`

### New functionality testing

**`test_policy` (MUST): Met.**
"New behaviour arrives with its tests, in the same change"; bug fixes start
from a failing test.
`REPO/blob/main/CONTRIBUTING.md#testing-policy`

**`tests_are_added` (MUST): Met.**
Enforced by review and by the traceability check: protocol behaviour without a
`covers:` test fails the conformance gate. Point to recent pull requests once
there are some.

**`tests_documented_added` (SUGGESTED): Met.**
CONTRIBUTING.md testing policy, and the pull request template checklist.
`REPO/blob/main/.github/PULL_REQUEST_TEMPLATE.md`

### Warning flags

**`warnings` (MUST): Met.**
rustc warnings and clippy, with `-D warnings`, on default and all features, on
every push.
`REPO/blob/main/.github/workflows/dev.yml`

**`warnings_fixed` (MUST): Met.** `-D warnings` makes any warning a build failure.

**`warnings_strict` (SUGGESTED): Met.**
`-D warnings` for clippy and rustdoc (including broken intra-doc links),
`missing_docs = "deny"`, `unsafe_code = "forbid"`.

---

## Security

### Secure development knowledge

**`know_secure_design` (MUST): Met. Owner to confirm.**
Evidence in the project: [`ASSURANCE_CASE.md`](ASSURANCE_CASE.md) (threat
model, least privilege in CI, defence in depth); treating all peer input as
untrusted; bounded buffers; `unsafe` forbidden; minimal default dependencies.

**`know_common_errors` (MUST): Met. Owner to confirm.**
The errors relevant to this software (parser panics, resource exhaustion,
super-linear parsing, state confusion, credential leakage, supply-chain
compromise) are named in ASSURANCE_CASE.md with the mitigation for each:
fuzzing, the perf gate, the lifecycle verifier, cargo-deny, pinned actions.

### Use basic good cryptographic practices

The crate implements no cryptography. TLS for the optional HTTP client is
delegated to rustls (feature `rustls`) or the platform TLS stack (feature `native-tls`), both through hyper connectors.

**`crypto_published` (MUST): Met.** Only published, reviewed protocols are used
(TLS, via rustls / platform stack). [JUDGEMENT] N/A is also defensible because
the crate itself implements none; Met is the more accurate answer since the
client feature does use TLS.

**`crypto_call` (SHOULD): Met.** Cryptography is never reimplemented; it is
called from rustls or the platform stack.

**`crypto_floss` (MUST): Met.** rustls is FLOSS; the platform stacks used by
`native-tls` (OpenSSL, SChannel, Security.framework) are the system's own,
and the default is rustls.

**`crypto_keylength` (MUST): N/A.** The crate chooses no key lengths; the TLS
library's defaults apply.

**`crypto_working` (MUST): N/A.** No cryptographic algorithms are selected by
this crate.

**`crypto_weaknesses` (SHOULD): N/A.** As above.

**`crypto_pfs` (SHOULD): N/A.** Key agreement is the TLS library's; rustls
supports only forward-secret suites.

**`crypto_password_storage` (MUST): N/A.** No passwords are stored.

**`crypto_random` (MUST): N/A.** No keys or nonces are generated by this crate.

### Secured delivery against man-in-the-middle (MITM) attacks

**`delivery_mitm` (MUST): Met.**
Delivered through crates.io over HTTPS with checksums in the index; source on
GitHub over HTTPS/SSH. Release artifacts carry SLSA provenance and cosign
signatures.

**`delivery_unsigned` (MUST): Met.**
No hash is fetched over HTTP; Cargo verifies crate checksums from the
HTTPS-served index.

### Publicly known vulnerabilities fixed

**`vulnerabilities_fixed_60_days` (MUST): Met.**
No unpatched vulnerabilities of medium or higher severity have been public for
more than 60 days. `cargo deny` checks RustSec on every push and weekly.

**`vulnerabilities_critical_fixed` (SHOULD): Met.**
SECURITY.md: fixes are released, not held, on the latest 0.9.x.

### Other security issues

**`no_leaked_credentials` (MUST): Met.**
No credentials in the repository. Publishing uses crates.io Trusted Publishing
(OIDC); the one bootstrap token lives in a GitHub environment secret and is
deleted after first use. Secret scanning and push protection are enabled.

---

## Analysis

### Static code analysis

**`static_analysis` (MUST): Met.**
Clippy with `-D warnings`, on default and all features, on every push and
pull request; `cargo deny` for dependencies.
`REPO/blob/main/.github/workflows/dev.yml`

**`static_analysis_common_vulnerabilities` (SUGGESTED): Unmet.**
Clippy is a correctness and idiom linter, not a vulnerability scanner.
Closing it: add CodeQL (`language: rust`) to the qa gate (on ROADMAP.md).
[JUDGEMENT] `cargo deny`'s RustSec check could be argued to qualify, but it
analyses dependencies, not this code.

**`static_analysis_fixed` (MUST): Met.** `-D warnings`: every finding fails the
build, so none can be left unfixed.

**`static_analysis_often` (SUGGESTED): Met.** On every push and pull request.

### Dynamic code analysis

**`dynamic_analysis` (SUGGESTED): Met.**
`cargo-fuzz` (libFuzzer with sanitizer coverage) over every parser of peer
input; a smoke run of every target in every qa gate.
`REPO/blob/main/.github/workflows/qa.yml`

**`dynamic_analysis_unsafe` (SUGGESTED): N/A.**
The crate is not written in a memory-unsafe language and forbids `unsafe`.

**`dynamic_analysis_enable_assertions` (SUGGESTED): Met.**
Tests and fuzzing run with debug assertions and overflow checks on (the
default for test and fuzz builds).

**`dynamic_analysis_fixed` (MUST): Met** if any fuzz finding has been fixed
(crash reproducers become regression tests); **N/A** if fuzzing has found
nothing of medium or higher severity yet.

---

## Summary

| Answer | Count | Notes |
| --- | --- | --- |
| Met | 57 | includes 2 owner-to-confirm, 2 that become Met with the first release, 2 [JUDGEMENT] on wording while there is no report history, and `dynamic_analysis_fixed` (Met, or N/A until fuzzing finds something) |
| N/A | 9 | 6 crypto, `release_notes_vulns`, `vulnerability_report_response`, `dynamic_analysis_unsafe` |
| Unmet | 1 | `static_analysis_common_vulnerabilities` (SUGGESTED; does not block) |
| **Total** | **67** | |

No MUST is Unmet, so the passing badge is available once SETUP.md steps 0–1
are done and the first release is tagged.
