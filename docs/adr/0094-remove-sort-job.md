# ADR-0094: remove `pigeon job run sort`

- **Author**: Willow Graysen ([@noisypigeon](https://github.com/noisypigeon)).
- **Date**: 2026-10-03.
- **Status**: Accepted.

## Context

`pigeon job run sort` (ADR-0083) downloads a bucket, flattens it into
top-level `<extension>/` folders by each file's literal, lowercased
extension, and uploads the result to a destination bucket — deliberately
with no content-hash dedup, no zip expansion, and no encryption, on the
stated assumption that it runs *after* a `pigeon job run dedupe` pass has
already established uniqueness.

`dedupe` (ADR-0082) already performs this exact same extension-flattening
as part of its own placement step: `dedupe/dedup.rs::place_one` and
`sort/worker.rs::place_one` are the same `result_dir.join(extension)` +
`sanitize_filename` + `unique_path` logic, and both jobs' `extension_of()`
are byte-for-byte identical (lowercase, `"(none)"` for no extension). The
only thing `sort` adds on top of what `dedupe` already produces is the
ability to flatten a bucket that is *known* unique but not yet organized
by extension, without re-paying for a content-hash pass.

In practice, every bucket ever handed to `sort` has already been through
`dedupe` first — which means it arrives *already* organized into
`<extension>/` folders, since that's `dedupe`'s own output layout. `sort`'s
one distinguishing capability is therefore never actually exercised: all
files go through the same path (`dedupe`, which both dedupes and organizes
in one pass), making a separate `sort` job dead weight — a second job type,
CLI surface, and test suite duplicating logic `dedupe` already performs as
a side effect.

ADR-0083 itself flagged the sharp edge this redundancy was always sitting
next to (its own Consequences section): "nothing enforces that `sort`'s
source bucket was actually produced by a prior `dedupe` run... pointing
`sort` at a bucket that *does* contain genuine content duplicates doesn't
corrupt anything, it just doesn't deduplicate them." Removing `sort`
removes that foot-gun along with the redundant code path.

No prior ADR proposed removing `sort` — confirmed via a full `docs/adr/`
sweep that found no deprecation signal anywhere for it. This is the first.
Per this repo's standing convention (CLAUDE.md: an architectural change
that contradicts an existing ADR gets a new ADR, not silent drift), this
ADR documents that decision explicitly rather than quietly deleting the
job.

## Decision

Delete `src/commands/job/sort/` wholesale and prune every reference to it.
Confirmed by inventory that the dependency between `sort` and the rest of
the codebase is one-directional: `sort` reused `commands/job/download.rs`,
`commands/job/upload.rs`, `commands/job/shared_wizard.rs`, and
`core::data`'s helpers, but nothing outside `sort/` imports anything
`sort`-specific. This is a clean deletion, not a refactor.

### Code

- Delete `src/commands/job/sort/{mod,wizard,worker,manifest}.rs`.
- `src/commands/job/mod.rs`: remove `pub mod sort;`.
- `src/commands/job/cli.rs`: remove the `JobType::Sort { .. }` variant and
  its `Observable::command_name()` arm (`"job.sort"`).
- `src/commands/job/commands.rs`: remove `sort` from the job-module `use`
  list and its dispatch match arm.
- `tests/cli.rs`: remove the 7 sort-specific integration tests
  (`job_run_help_lists_sort` and six `job_run_sort_*` tests covering its
  help text, `--upload-only` preflight, and non-interactive failure
  paths).
- `src/observability/metrics.rs`: `record_phase`'s doc comment used
  "sort's `download`/`placement`" as its example of phase-name reuse
  across jobs — swapped to `dedupe`'s, since `sort` no longer exists to
  reference.

### Documentation

- `docs/adr/0083-sort-job.md`: `Status` marked `Reversed by ADR-0094`
  (same pattern as ADR-0035's `Rejected` marking) — body left untouched as
  the historical record of why the job existed and how it worked.
- `CLAUDE.md`: ADR-0083's digest bullet gets a trailing reversal note; a
  new bullet is added for this ADR. ADR-0090/0091's own bullets, which
  mention `sort` as one of several jobs they touched at the time, are left
  as-is — they're accurate history of what those ADRs did, not claims
  about current state.
- `.claude/skills/analyze-job-run/SKILL.md`: removed `job.sort` from the
  documented `command_name()` values, the `sort: download, place` step
  vocabulary entry, and the `sort:` completion-summary format string —
  all three would otherwise describe a command that no longer exists.
- `CHANGELOG.md`: one new `[Unreleased]` bullet for this ADR. The existing
  ADR-0083/0090/0091 entries are left untouched — they're an accurate
  record of what shipped at the time, not a live inventory of current
  commands.
- `README.md`: no change — it never listed `sort` (its Commands section
  predates and remains incomplete for several other job types too,
  unrelated to this removal).

## Consequences

- `pigeon` goes from 5 upload-capable job types to 4
  (`email-sync`/`email-pull`/`pull-transform`/`dedupe`); `decrypt-files`
  remains the one non-upload job.
- A user who genuinely wants `sort`'s narrow behavior — flatten a bucket
  that's already known-unique, without paying for a redundant SHA-256
  pass — loses that specific option. They can still run `dedupe`, whose
  only extra cost on top of what `sort` did is the content hash itself;
  against the download/upload I/O that already dominates both jobs' wall
  time, this is a deliberate, accepted tradeoff, not an oversight.
- GitHub issue [noisypigeon/noisypigeon-2#97](https://github.com/noisypigeon/noisypigeon-2/issues/97)
  (ADR-0083's deferred "generalize the bucket-listing/`TypeSummary`/
  checkpoint pattern, duplicated three times" item, which named `sort` as
  one of the three) now has one fewer duplicate implementation to
  generalize (`pull_transform::manifest`, `dedupe::manifest` remain). No
  action taken on the issue itself here, noted for continuity.
- ADR-0092/0093's Prometheus metrics, scoped to "the five upload-capable
  jobs," now cover four; nothing in the metrics code itself hardcodes a
  job count or name list (`record_phase`/`set_macro_phase` take a plain
  `&'static str` label), so no metrics code change beyond the doc-comment
  example was needed.

## Verification

- `mise run ci` clean (fmt, clippy, full test suite minus the 7 removed
  tests).
- `pigeon job run --help` no longer lists `sort`; `pigeon job run sort
  --help` fails as an unrecognized subcommand.
- Repo-wide grep for `"sort"` as a job-name/command-name string (excluding
  `Vec::sort`/`.sort_by`/`.sort_unstable` calls and now-historical prose in
  `docs/adr/0083`, `CLAUDE.md`, `CHANGELOG.md`) turns up nothing left to
  update.
