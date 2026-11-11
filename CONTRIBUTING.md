# Contributing to agui-sdk

## Branches are the pipeline

Three long-lived branches, in one direction only, and releases cut by tag:

```
dev  ──fast-forward──▶  qa  ──fast-forward──▶  main  ──tag vX.Y.Z──▶  crates.io
 │                       │                      │                      │
 fast gate          full gate              released code          release.yml
```

- **`dev`**: where work lands, and what pull requests target. Every push and
  every pull request runs `.github/workflows/dev.yml`: rustfmt, clippy with
  `-D warnings` on the workspace (where the test harness enables `client`,
  `server` and `proto`), on the library alone with no features, and on
  `--all-features`, the workspace tests both ways, doctests, and `cargo deny`, on latest stable Linux. It is deliberately
  a few minutes: it is the inner loop.
- **`qa`**: advanced by fast-forwarding from a `dev` commit that already
  passed. `.github/workflows/qa.yml` is the thorough gate and green `qa` is the
  release signal. It adds:
  - the test set on Linux, macOS and Windows;
  - the MSRV floor (read from `Cargo.toml`, never hardcoded) for the core
    crate on all three OSes, and the library alone with no features and with
    each optional feature by itself, checked at its documented floor and
    clippied (Linux; tests there run with the feature plus `client`,
    `server` and `proto`, which the test harness enables);
  - `cargo doc` with `-D warnings` and dead intra-doc links as failures;
  - **spec-sync**: the upstream AG-UI spec is fetched into `.spec-cache/` and
    the conformance suite runs against it ([`docs/CONFORMANCE.md`](docs/CONFORMANCE.md));
  - a fuzz smoke run of every `cargo-fuzz` target;
  - the **perf gate** (`perf.yml`): throughput, allocation-count and linearity
    thresholds ([`docs/PERFORMANCE.md`](docs/PERFORMANCE.md));
  - a publish dry run of both crates, coverage, and `cargo-semver-checks`.
- **`main`**: advanced by fast-forwarding from a green `qa`. Releases are tags
  on `main`.

## Before you push

Run locally what touches your change; CI runs the rest.

```sh
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test -p agui-sdk <the tests your change touches>
```

There is no need to run the whole matrix, the fuzzers or the perf gate
locally; `qa` does that. If you changed a hot path, run the relevant bench
(see [`docs/PERFORMANCE.md`](docs/PERFORMANCE.md)).

## Commits

- **Conventional Commits.** `type(scope): summary`, with `feat`, `fix`,
  `perf`, `refactor`, `test`, `docs`, `build`, `ci` or `chore`. A breaking
  change says so with `!` and a `BREAKING CHANGE:` footer. The history is what
  the changelog is written from, so a summary should say what changed for a
  user, not which file moved.
- **Sign off every commit** (Developer Certificate of Origin, below):
  `git commit -s`.
- **No AI attribution lines.** No `Co-Authored-By:` naming a tool or model, no
  "Generated with …" trailers, in commits or pull request descriptions. Use
  whatever tools you like; the commit is signed off by you, and you are the
  one answerable for it. Commits carrying such lines are asked to be amended.

## Spec traceability

Every normative requirement in the AG-UI 1.0 specification has an ID, listed
in [`docs/SPEC-MAP.md`](docs/SPEC-MAP.md). A test that checks a requirement
says so with a doc comment naming the ID:

```rust
/// covers: PROC-31
#[test]
fn second_run_started_before_finish_is_rejected() { /* ... */ }
```

One test may cover several IDs (`/// covers: PROC-31, VER-11`). The
conformance gate cross-checks the tags against the map: an ID with no covering
test, or a tag naming an ID that does not exist, fails CI. A change that
implements protocol behaviour without a `covers:` tag is not finished. The IDs
above are examples; `docs/SPEC-MAP.md` is the authoritative list.

## Coding standards

Machine-checked, on every push and every pull request:

| Standard | Enforced by |
| --- | --- |
| One formatting | `cargo fmt --all --check` (`rustfmt.toml`) |
| No warnings | `cargo clippy --workspace --all-targets [--all-features] -- -D warnings`, and `cargo clippy -p agui-sdk --lib --no-default-features [--features <f>] -- -D warnings` with no features and each feature alone (qa `msrv-features`, D15), with `clippy.toml` pinning `msrv` so MSRV-aware lints stay correct |
| No `unsafe` | `#![forbid(unsafe_code)]` in every library crate, checked in qa |
| Every public item documented | `#![deny(missing_docs)]`, and `cargo doc` with `-D warnings` in qa |
| Builds on the MSRV | the qa `msrv` and `msrv-features` jobs |
| Licences and advisories | `cargo deny check` (`deny.toml`) |

Beyond the table: peer input is untrusted, so a malformed event is an error and
never a panic; comments explain *why*; and nothing is allowed to fail quietly.
There is no `continue-on-error` in any workflow, because a check that may fail
is not a check.

## Minimal dependencies

Every dependency is code you did not write running in someone else's agent.
So:

- **Every new dependency needs a written justification in
  [`docs/API.md`](docs/API.md)**, in the same pull request: what it does, why
  the standard library or an existing dependency will not do, which feature
  it sits behind, and what it costs (transitive crates, MSRV).
- The default feature set stays at the protocol types plus `serde` and
  `serde_json`. Anything else goes behind a feature.
- Prefer `default-features = false` and enable only what is used.
- `cargo deny` rejects disallowed licences, known advisories, and git or
  unknown-registry sources.

## Testing policy

- **New behaviour arrives with its tests, in the same change.**
- **A bug fix starts with a failing test.** Write the test that reproduces the
  defect, watch it fail, then fix. Say in the pull request what was red.
- **Assert against the spec, not the implementation.** A test whose expected
  value is computed by the same logic as the code under test cannot fail.
  Expected events and wire bytes come from the specification or its fixtures.
- **Tests run from a checkout.** The published `.crate` carries the library
  sources only; its unit tests need the repository's `tests/proto_support`
  and the pinned spec cache (D6), so `cargo test` on an unpacked package does
  not build.
- **Hot-path changes keep their budgets.** The perf gate fails on an
  allocation-count or linearity regression; if a change legitimately moves a
  budget, move it in the same pull request and say why.

## Cutting a release

1. On `dev`: bump `[workspace.package] version` in the root `Cargo.toml` and
   the alias crate's exact `agui-sdk = "=X.Y.Z"` requirement in
   `crates/ag-ui-sdk/Cargo.toml` (both crates move together), and turn
   the `## [Unreleased]` section of `CHANGELOG.md` into
   `## [X.Y.Z] - YYYY-MM-DD`, adding the `[X.Y.Z]` tag link at the bottom.
   A version's section says what that version ships, relative to the previous
   published release; changes made and undone before it was published are not
   narrated, and maintainer instructions stay here, not in the section (it
   becomes the GitHub Release body). The release refuses a version with no
   changelog section, or whose heading is not `## [X.Y.Z] - YYYY-MM-DD`.
2. Fast-forward `qa`; wait for `qa gate` to go green.
3. Fast-forward `main`, then tag it: `git tag -s vX.Y.Z && git push origin vX.Y.Z`.
   `release.yml` re-runs the whole qa gate on the tagged commit, refuses a tag
   that is not on `main` or does not match the manifest, publishes `agui-sdk`
   then `ag-ui-sdk`, and opens a GitHub Release with signed, attested `.crate`
   files. A re-run after a partial failure skips whatever already happened.

One-time owner setup (crates.io authentication, branch rulesets, secrets) is in
[`docs/SETUP.md`](docs/SETUP.md).

## Conventions in `.github/`

- Every third-party action is pinned to a **full commit SHA** with the readable
  ref in a trailing comment. Dependabot bumps the pins monthly onto `dev`.
- `permissions:` is explicit and minimal in every workflow, raised per job only
  where needed.
- No `${{ }}` inside `run:` blocks; values reach the shell through `env:`.
- No `continue-on-error`.

## Developer Certificate of Origin

Contributions are accepted under the
[Developer Certificate of Origin 1.1](https://developercertificate.org/). There
is no CLA and no copyright assignment: the sign-off states that you wrote the
change, or otherwise have the right to submit it under this project's MIT
licence.

```sh
git commit -s
```

appends `Signed-off-by: Your Name <you@example.com>`. Use a real name and a
reachable address. `git commit --amend -s` fixes the last commit and
`git rebase --signoff` fixes a branch.
