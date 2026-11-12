# One-time owner setup

Everything a workflow cannot do for itself, in the order to do it. Each step is
a click-through behind an interactive login, which is why none of it is
automated. Register everything as **Matthew Jackson, matthew@pq.io**.

Nothing here blocks development: `dev` is green without any of it. Steps 1–3
should be done before the repository is public; step 4 blocks the first
release; steps 5–6 add badges.

---

## 0. Create the repository and push the branches

Create `MattJackson/agui-sdk` (public), push `dev`, then fast-forward and push
`qa` and `main`. Set **`main` as the default branch** (Scorecard and the REUSE
badge read the default branch, and the release workflow checks tags against
`main`).

Two README badges go live on their own after the push, with no account:

- **OpenSSF Scorecard**: "unknown" until the first run of
  `.github/workflows/scorecard.yml` (weekly, Mondays). Run it once from
  Actions → scorecard → Run workflow to get it immediately.
- **REUSE**: "unknown" until api.reuse.software scans the repository. Loading
  <https://api.reuse.software/info/github.com/MattJackson/agui-sdk> once
  triggers the scan.

---

## 1. Private vulnerability reporting

**Settings → Code security → Private vulnerability reporting → Enable.**

`SECURITY.md` names this as the reporting route, and the OpenSSF questionnaire
answers `vulnerability_report_process` and `vulnerability_report_private`
against it. Until it is enabled the link in `SECURITY.md` is dead.

While on that page, also enable:

- **Dependabot alerts** and **Dependabot security updates** (version updates
  are already configured in `.github/dependabot.yml`);
- **Secret scanning** and **Push protection**.

---

## 2. Branch and tag rulesets

**Settings → Rules → Rulesets → New branch ruleset**, one per branch. GitHub
lists a status check as selectable only after it has run once on the repo, so
push first (step 0).

| Ruleset | Target | Rules |
| --- | --- | --- |
| `dev` | branch `dev` | Restrict deletions; block force pushes; require status checks: `fmt, clippy, test` and `cargo-deny (advisories, licences, bans, sources)` (from dev.yml); bypass list: repository admin, mode "Always" |
| `qa` | branch `qa` | Restrict deletions; block force pushes; require linear history |
| `main` | branch `main` | Restrict deletions; block force pushes; require linear history; require status check `qa gate` |
| `release tags` | tag pattern `v*` | Restrict creations, updates and deletions to repository admins (add yourself to the bypass list) |

`qa gate` is the single aggregate job in `qa.yml` built to be the one required
check: it goes red if any qa job failed, was cancelled or was skipped.

A required status check gates the ref update itself: the commit must already
carry the passing check before the push is accepted. So `qa gate` is required
only on `main`, where every commit fast-forwarded in has already passed
`qa gate` on `qa`. On `qa` it could never be satisfied, because `qa gate` runs
only after the push to `qa` it would be gating; `qa` is checked after the push
by qa.yml, and that result is enforced when the commit reaches `main`. On
`dev` the required checks stay so that pull requests (Dependabot,
contributors) merge only green; the owner pushes to `dev` directly through
the admin bypass, and dev.yml still runs on that push.

Optional for a solo maintainer, worth it once there is a second: "Require a
pull request before merging" on `main`. Scorecard's Branch-Protection and
Code-Review checks score higher with it.

---

## 3. The `release` environment

**Settings → Environments → New environment → `release`.**

- **Required reviewers**: yourself. Every publish then waits for a click,
  after the full qa gate has passed on the tag.
- **Deployment branches and tags**: "Selected branches and tags", add the tag
  pattern `v*`.

The release job runs in this environment, and crates.io's trusted-publisher
entry (step 4) names it, so a workflow anywhere else cannot mint a publish
token.

---

## 4. crates.io publishing (blocks the first release)

`release.yml` publishes `agui-sdk` and then `ag-ui-sdk`. It uses the
`CRATES_IO_TOKEN` secret if one exists, and crates.io **Trusted Publishing**
(OIDC, no long-lived secret) otherwise.

At the time of writing, crates.io only lets you configure Trusted Publishing
for a crate that **already exists**, so the first release needs a token:

1. At <https://crates.io/settings/tokens>, create a token named
   `agui-sdk first publish`, scope **publish-new** and **publish-update**,
   restricted to crates `agui-sdk` and `ag-ui-sdk`, with a short expiry.
2. **Settings → Environments → `release` → Environment secrets → Add**:
   `CRATES_IO_TOKEN` with that value. (An environment secret, not a repository
   secret, so only the `release` environment can read it.)
3. Cut the first release (CONTRIBUTING.md, "Cutting a release").
4. Then, for **each** crate, at `https://crates.io/crates/<crate>/settings` →
   Trusted Publishing → Add, GitHub, with exactly:

   | field | value |
   | --- | --- |
   | Repository owner | `MattJackson` |
   | Repository name | `agui-sdk` |
   | Workflow filename | `release.yml` |
   | Environment | `release` |

5. **Delete the `CRATES_IO_TOKEN` secret** and revoke the token on crates.io.
   From then on every release mints a ~30-minute token per run and nothing
   long-lived exists anywhere.

If crates.io supports trusted publishing for new crates by the time you do
this, skip 1–3 and 5: configure step 4 first ("pending publisher") and tag.

---

## 5. Codecov

The coverage job runs whether or not Codecov exists; Codecov adds history and
a badge.

1. Sign in at <https://app.codecov.io> with GitHub and activate
   **agui-sdk**.
2. Copy the upload token from the repository's Codecov settings.
3. **Settings → Secrets and variables → Actions → New repository secret**:
   `CODECOV_TOKEN`.
4. After the first successful upload, uncomment the codecov badge in
   `README.md` (it is in an HTML comment under the badge block).

No workflow change is needed: the upload step is guarded on the secret
existing and skips itself, with a note in the run summary, until it does.

---

## 6. OpenSSF Best Practices badge

The answers are drafted in [`openssf-best-practices.md`](openssf-best-practices.md),
in the form's order. Do steps 0–1 first: several answers cite `blob/main/...`
URLs and the private reporting route.

1. Sign in at <https://www.bestpractices.dev> with GitHub.
2. **Add project**: project and repository URL both
   `https://github.com/MattJackson/agui-sdk`.
3. Work down the draft. Read the entries marked **owner to confirm** (they are
   claims about you, not the repository) and **[JUDGEMENT]** before submitting.
4. The site assigns a numeric project id. In `README.md`, replace `NNNNN` in
   the commented-out OpenSSF Best Practices badge with it, uncomment it, and
   move it directly after the `unsafe: forbidden` badge.

---

## Summary of secrets and settings

| Name | Where | Needed for | Lifetime |
| --- | --- | --- | --- |
| `CRATES_IO_TOKEN` | `release` environment secret | first publish of each crate | delete after Trusted Publishing is configured |
| `CODECOV_TOKEN` | repository secret | coverage upload | permanent (optional) |
| `release` environment | Settings → Environments | publish job, OIDC identity | permanent |
| Private vulnerability reporting | Settings → Code security | SECURITY.md route | permanent |
