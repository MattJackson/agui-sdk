<!-- Target `dev`. Read CONTRIBUTING.md first; CI enforces most of it. -->

## What and why

<!-- One paragraph. Link the issue. -->

## Spec traceability

<!-- Requirement IDs from docs/SPEC-MAP.md this change implements or tests,
     each with a `/// covers: ID` tag on the test. "None" is a fine answer for
     non-protocol changes. -->

## Checklist

- [ ] Commit messages follow Conventional Commits, and every commit is signed off (`git commit -s`).
- [ ] No AI attribution lines (`Co-Authored-By:` a tool, "Generated with ...") in any commit.
- [ ] `cargo fmt --all --check` and `cargo clippy --workspace --all-targets --all-features -- -D warnings` are clean.
- [ ] New behaviour has tests in this PR; a bug fix starts with a test that failed before the fix (say which).
- [ ] Any new dependency is justified in docs/API.md (CONTRIBUTING.md, "Minimal dependencies").
- [ ] Hot-path changes: the perf gate's allocation budgets still hold (docs/PERFORMANCE.md).
- [ ] CHANGELOG.md `Unreleased` updated for anything user-visible.
