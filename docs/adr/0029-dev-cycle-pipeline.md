# ADR-0029: ADR-driven dev cycle — branch, PR, auto-merge, changelog

- **Author**: Willow Finch ([@noisypigeon](https://github.com/noisypigeon)).
- **Date**: 2026-09-25.
- **Status**: Accepted.

## Context

Every ADR through 0028 landed as direct changes against `main`, with no branch, PR, or changelog-per-change history — fine while establishing the product, but the sole contributor here wants every substantive change from here on to go through a real PR: an audit trail (what changed, why, linked to its ADR), not a second-reviewer gate that doesn't exist. The PR itself should not add friction — no manual "approve and click merge" step beyond what already happens when the ADR (or implementation plan) is approved.

This repo has no CI infrastructure yet (`.github/workflows/` is empty) and no branch protection. Building real GitHub Actions CI plus native auto-merge is a reasonable future upgrade but isn't required to get a working, low-friction pipeline today — the existing `mise run ci` local gate already covers fmt/lint/test, and is the only gate this ADR adds to the merge step.

## Decision

### 1. One branch per ADR (or per later implementation of one), from up-to-date `main`

`git checkout main && git pull && git checkout -b adr-XXXX-<slug>`, created right after the ADR's content — or, for a later "now implement ADR-XXXX" request, the implementation plan — is approved. The existing Plan Mode / `ExitPlanMode` approval already in use for every ADR this session *is* the "written and approved" checkpoint; no new approval tooling is introduced. One branch in flight at a time — no stacking.

### 2. Local CI gate before a PR is even opened

`mise run ci` (fmt-check + clippy `-D warnings` + test) must pass before `gh pr create` runs at all. A failing gate blocks opening the PR, not just merging it.

### 3. PR description is a condensed ADR, not the full text

`gh pr create` with a title matching this repo's existing commit convention (`type(adr-XXXX): summary`, e.g. `feat(adr-0024): concurrent uploads and better observability`, per current `git log`). Body: the ADR's Context in 1-2 sentences, its Decision as condensed bullets, and a relative link to the full ADR file in `docs/adr/` — never the whole ADR pasted in.

### 4. `service/pigeon-cli/CHANGELOG.md` gains an `[Unreleased]` section, updated once per merged PR

Keep a Changelog's standard `[Unreleased]` section, added at the top of the file (above `[0.2.0]`). Once a PR exists (so its number/URL is known), push one follow-up commit on the same branch adding a single bullet:

```
- ADR-XXXX: <one-line description> ([#N](https://github.com/noisypigeon/pigeon/pull/N))
```

This is why the changelog update happens *after* PR creation, not before — the PR URL doesn't exist yet at branch-creation time.

### 5. Merge is automatic once the gate is clean — no further confirmation

`gh pr merge --squash --delete-branch` once `mise run ci` is clean on the branch. This is the specific step this ADR exists to pre-authorize: no additional "should I merge this?" check per PR. Squash is also the only merge method this repo's settings allow.

### 6. Releases stay manual, unchanged from current practice

`[Unreleased]` accumulates bullets across however many PRs land between releases. Cutting a release is still a manual, separate act: retitle `[Unreleased]` to `[x.y.z] - date` with a short summary blurb (as already done for 0.1.0/0.2.0), and start a fresh empty `[Unreleased]` section above it. Nothing here automates that step.

## Consequences

- Every substantive change gets a real PR, branch, and changelog line going forward — a genuine audit trail where today there's a flat commit history with no PR record at all.
- No manual merge-approval friction: the ADR/plan approval step (already required) is the only human checkpoint; CI passing is the only automated one.
- `service/pigeon-cli/CHANGELOG.md` becomes a living document updated continuously, not just at release time — `[Unreleased]` is always an accurate "what's landed since the last release" list.
- No new CI infrastructure or repo-settings changes (branch protection, required status checks) — the gate is local and self-administered, which is only appropriate because there's no second contributor to bypass it.

## Out of scope

- Real GitHub Actions CI and GitHub's native auto-merge (viable later upgrade if a second contributor or a desire for a visible status check ever arrives). ([#34](https://github.com/noisypigeon/pigeon/issues/34))
- Automating the release-cutting step itself. ([#35](https://github.com/noisypigeon/pigeon/issues/35))
- Any form of code review from a second person — there isn't one.

Implementation is part of this same task (small enough not to defer, and this ADR's own landing is the first real exercise of the pipeline it defines).
