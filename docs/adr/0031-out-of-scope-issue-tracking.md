# ADR-0031: GitHub-issue tracking for ADR Out of scope items

- **Author**: Willow Finch ([@noisypigeon](https://github.com/noisypigeon)).
- **Date**: 2026-09-25.
- **Status**: Accepted.

## Context

Every ADR from 0002 onward carries an `## Out of scope` section, and a lot of those bullets are genuine deferred work -- not permanent design boundaries -- that nobody has been tracking anywhere. They just sit in a Markdown file, with no way to see them as a backlog, prioritize them, or notice when the same gap has been raised more than once. GitHub Issues is now enabled on this repo, which makes it possible to close that gap without inventing new infrastructure.

The goal isn't to implement any of these deferred items now -- it's to make them visible and prioritizable "once they become a blocker," per the original ask, instead of staying buried in ADR prose.

## Decision

### 1. Category labels

Seven labels, one tracking label plus one per subsystem, matching the module boundaries already established by ADR-0008/0017/0022/0023:

- `out-of-scope` -- applied to every issue filed from an ADR's Out of scope section.
- `area:email-sync` -- IMAP sink/transform/sync pipeline.
- `area:dataops` -- S3-compatible remote storage (bucket configs, upload).
- `area:keyring` -- identities, bucket configs, and encryption keys in the OS keychain.
- `area:job-orchestration` -- the `job` wizard, concurrency, and phase orchestration.
- `area:security` -- encryption, key management, and related crypto.
- `area:tooling` -- dev-cycle, CI, release, and publishing tooling.

### 2. `scripts/adr-issue.sh` + `mise run adr-issue`

New machinery, kept as repo-local dev tooling rather than a `pigeon` subcommand -- this is process bookkeeping for the maintainer, not part of the shipped email/dataops product, the same reasoning ADR-0029 used to keep its dev-cycle pipeline as pure process plus mise tasks instead of new CLI surface.

```
mise run adr-issue -- \
  --title "Support non-S3 dataops backends (GCS, Azure, local-to-local)" \
  --body "Raised in ADR-0009 as future work..." \
  --label out-of-scope --label area:dataops \
  --adr-line docs/adr/0009-remote-storage.md:76 \
  [--adr-line docs/adr/0011-....md:53]   # repeatable, for items raised in >1 ADR
  [--issue 17]                            # reuse an existing issue instead of creating one
```

It: ensures each `--label` exists (idempotent `gh label create`, using a built-in color/description table for the seven labels above); creates a new issue via `gh issue create`, or resolves an existing one's URL when `--issue N` is passed instead; then, for every `--adr-line file:line`, appends `` ([#N](issue-url))`` to the end of that exact line in place. That last step is what puts the issue link directly in the ADR's Out of scope bullet, and appending (rather than inserting a line) keeps every other line number in the file stable across repeated runs.

### 3. Reuse one issue across ADRs

When the same gap is raised by more than one ADR (e.g. "no delete/mirror semantics" shows up in both ADR-0009 and ADR-0011), file it once and pass multiple `--adr-line` flags, or attach a later ADR's line to the already-filed issue with `--issue N`. The label taxonomy and the script's `--issue` reuse path exist specifically so recurring gaps collapse into one tracked item instead of duplicating.

### 4. Going forward

When a new ADR's `## Out of scope` section contains a genuinely-deferred item -- not a permanent design boundary like "no OAuth2 in this ADR's scope" or "no code review, there isn't a second contributor" -- run `mise run adr-issue` for it while landing that ADR, checking first whether an existing `out-of-scope`-labeled issue already covers the same gap.

### 5. One-time backfill

Every ADR from 0003 through 0030 was audited against the current codebase for Out of scope bullets that are still genuinely unaddressed (as opposed to resolved by a later ADR, or a permanent boundary). Each surviving item was filed as an issue via this same machinery and linked back into its originating ADR line(s), collapsing duplicates per the reuse rule above.

## Consequences

- Every still-open Out of scope bullet across ADR-0003-0030 gains an inline issue link, e.g. `- Support non-S3 dataops backends (GCS, Azure, local-to-local). ([#17](https://github.com/noisypigeon/pigeon-cli/issues/17))`.
- The repo gains its first `scripts/` directory and its first non-cargo mise task.
- Deferred work becomes a real, labeled, prioritizable backlog instead of prose that's easy to forget.
- Landing a future ADR gains one small extra step, offset by how cheap it is (one script call per deferred item).

## Out of scope

- Auto-detecting duplicate or overlapping issues -- reuse via `--issue` is a manual judgment call made at filing time, not automated matching.
- Priority or severity labels, or automatically escalating an issue when it "becomes a blocker" -- that's still a manual relabel/triage step later.
- Moving this machinery into the `pigeon` binary.
- Filing issues for permanent-by-design boundary statements (e.g. "no server-side mutation, enforced structurally") -- these aren't backlog items and never get an issue.

Implementation, including the one-time backfill across ADR-0003-0030, is part of this same task.
