# Governance

How `agui-sdk` is governed: who decides what, how a change gets in, who holds
the keys, and what happens if that person stops answering. It is short, and it
describes the project's real size rather than a structure that does not exist.

## Model

`agui-sdk` uses a **benevolent-maintainer** model, which is the accurate name
for "one person decides". There is one maintainer, no steering committee, and
no vote to appeal to.

Discussion happens in the open, on the issue tracker and on pull requests. The
maintainer makes the final call, guided by the priorities the repository
already states: conformance to the AG-UI 1.0 specification first, safety on
untrusted input second, and a small dependency footprint third.

**What this project does not govern is the protocol.** AG-UI is specified by
the AG-UI project (<https://github.com/ag-ui-protocol/ag-ui>), with which this
crate is not affiliated. Where this crate and the specification disagree, the
specification wins and the crate has a bug. Proposals to change the protocol
belong upstream.

If a second active maintainer appears, this document is rewritten to say how
the decision is shared (expected: lazy consensus on pull requests, either may
merge after review, disagreements resolved by not merging until they agree).

## Roles and responsibilities

- **Maintainer**: Matthew Jackson ([@MattJackson](https://github.com/MattJackson)),
  <matthew@pq.io>. Reviews and merges, advances `dev` → `qa` → `main`, tags
  releases, triages issues, handles security reports ([`SECURITY.md`](SECURITY.md)),
  and is the enforcement contact for [`CODE_OF_CONDUCT.md`](CODE_OF_CONDUCT.md).
  Holds every credential listed below.
- **Contributors**: anyone who opens an issue or pull request. What a
  contribution must satisfy is in [`CONTRIBUTING.md`](CONTRIBUTING.md); CI
  enforces nearly all of it.
- **Users**: anyone depending on the published crates. The most useful thing a
  user can send is a report of a real agent or frontend the crate could not
  talk to, with the events on the wire.

## How decisions are made

1. **Non-trivial changes start as an issue**, saying what the change buys
   someone building on AG-UI. Protocol behaviour cites the spec requirement it
   implements.
2. **Changes arrive as pull requests against `dev`**, which run the fast gate.
   A red gate is not a discussion.
3. **`qa` is the thorough gate**, and green `qa` is the release signal.
4. **Releases are tags on `main`**, which re-run the whole qa gate on the
   tagged commit before publishing. Mechanics: [`CONTRIBUTING.md`](CONTRIBUTING.md#cutting-a-release).
5. **1.0 is the maintainer's decision** ([`ROADMAP.md`](ROADMAP.md)).
6. **Security-relevant changes additionally follow [`SECURITY.md`](SECURITY.md)**:
   a private advisory and a released fix before details are public.

## Access and continuity

### Who holds what

- **GitHub repository admin**: the maintainer, alone, on
  <https://github.com/MattJackson/agui-sdk>, including rulesets, Actions
  secrets, environments, and the private vulnerability reporting channel.
- **crates.io ownership**: the maintainer is the sole owner of both
  [`agui-sdk`](https://crates.io/crates/agui-sdk) and
  [`ag-ui-sdk`](https://crates.io/crates/ag-ui-sdk). The names are the real
  keys to the project: a fork of the source is a click, and a crate name is not.
- **Publishing credentials**: releases authenticate with crates.io Trusted
  Publishing (OIDC) in the `release` environment, so no long-lived token needs
  to exist. A `CRATES_IO_TOKEN` secret is used only to bootstrap a crate's
  first release, and deleted afterwards ([`docs/SETUP.md`](docs/SETUP.md)).
- **Nothing is held off the record.** No credential is committed and there is
  no infrastructure beyond the repository and its workflows.

### Bus factor

**One.** Every role above is the same person. If that person is unreachable,
no new version can be published under these names, no advisory can be issued
here, and no pull request can be merged.

### If the maintainer becomes unavailable

- **Everything needed to continue is public**: source, tests, conformance
  harness, workflows, and the reasoning in `docs/`. The spec data is fetched
  from upstream, so nothing private is needed to rebuild or re-verify.
- **The licence permits it.** MIT: anyone may fork and publish under a new name.
- **Published versions keep working.** crates.io releases are immutable.
- **A fork cannot inherit the names.** crates.io's policy for abandoned crates,
  or a new name, is the successor's route.
- **The fix** is a second crates.io owner and repository admin. It is on
  [`ROADMAP.md`](ROADMAP.md) as a goal, not claimed as done.

## Changing this document

Like any other change: a pull request, gated by CI, merged by the maintainer.
If the governance described here stops being true, this file is the first
thing to update.
