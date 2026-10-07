# ADR-0103: remove `pigeon job run reduce`

- **Author**: Willow Graysen ([@noisypigeon](https://github.com/noisypigeon)).
- **Date**: 2026-10-07.
- **Status**: Accepted.

## Context

`pigeon job run reduce` (ADR-0096) runs after `deduplicate`: it scans a
source bucket already flattened into top-level `<extension>/` folders,
classifies each extension as either genuinely valuable or an artifact/piece
of media (TV, movie, software installer, disk image) that's easily
reproduced from an external canonical source, via a curated, hardcoded
extension table (`REPRODUCIBLE_EXTENSIONS` in
`src/commands/job/reduce/classify.rs`), and forwards only the valuable
extensions to a destination bucket.

The decision has been made to stop delegating that "is this worth keeping"
judgment to `reduce`'s hardcoded table. Going forward, the workflow is
`pigeon job run import` (ADR-0101) followed by `pigeon job run deduplicate`
(ADR-0082/0096) as two separate, manually-verified steps, with the
valuable/reproducible classification done by hand instead. `reduce` has no
remaining use.

No prior ADR proposed removing `reduce`. This repo has a direct precedent
for the exact same situation: **ADR-0094 removed `sort`**, a job that had
likewise been superseded by how the other jobs were actually being used,
and whose code had the same one-directional dependency shape on shared
infrastructure. Per `CLAUDE.md`'s standing rule (an architectural change
that contradicts an existing ADR gets a new ADR, not silent drift), this
ADR documents the removal explicitly and mirrors ADR-0094's structure.

## Decision

Delete `src/commands/job/reduce/` wholesale and prune every reference to it.
Confirmed one-directional dependency, same as `sort`'s: `reduce` reused
`commands/job/download.rs`, `upload.rs`, `shared_wizard.rs`, and
`core::data`'s helpers, but nothing outside `reduce/` imported anything
`reduce`-specific. Clean deletion, not a refactor.

`core::data::extension_of` and `shared_wizard::SourceBucketInput` were both
hoisted into shared modules specifically because `reduce` became their
*third* consumer (ADR-0096 §0). Confirmed both still have 2 consumers after
this removal (`deduplicate` and `pull_transform`) — no un-hoisting is
warranted, unlike `sort`'s removal, which *did* pull `ConcurrencyInput` back
out of `shared_wizard.rs` after it dropped to a single caller.

### Code

- Deleted `src/commands/job/reduce/{mod,classify,manifest,worker,wizard}.rs`.
- `src/commands/job/mod.rs`: removed `pub mod reduce;`.
- `src/commands/job/cli.rs`: removed the `JobType::Reduce { .. }` variant
  and its `Observable::command_name()` match arm (`"job.reduce"`).
  `job_name()` needed no change — it derives from `command_name()`.
- `src/commands/job/commands.rs`: removed `reduce` from the job-module
  `use` list and its dispatch match arm.
- `src/commands/job/import/wizard.rs`: dropped a doc-comment clause that
  compared `LocalOutputInput`'s shape to `reduce::wizard::LocalOutputInput`,
  which no longer exists.
- `tests/cli.rs`: removed the 6 reduce-specific integration tests
  (`job_run_help_lists_reduce` and five `job_run_reduce_*` tests covering
  its help text, `--upload-only` preflight, and non-interactive failure
  paths).
- `src/commands/job/report_upload.rs`: its generic `write_summary_report()`
  unit test happened to use the literal string `"reduce"` as an arbitrary
  example job name — swapped to `"pull-transform"`, a surviving job, so the
  test fixture doesn't reference a deleted job type.
- `src/observability/metrics.rs` / `src/core/job.rs`: confirmed no `reduce`
  references — nothing to change here (unlike `sort`'s removal, which
  needed a doc-comment example swapped).

### Documentation

- `docs/adr/0096-rename-dedupe-and-add-reduce-job.md`: `Status` marked
  `Reversed by ADR-0103` (ADR-0083/ADR-0094's exact pattern) — body left
  untouched as the historical record of why `reduce` existed and how it
  worked. Only the `reduce`-adding half of ADR-0096 is reversed; its
  unrelated `dedupe` → `deduplicate` rename is unaffected and stands.
- `CLAUDE.md`: ADR-0096's digest bullet gets a trailing reversal note; a
  new bullet is added for this ADR. ADR-0097/0098/0099/0100's own bullets,
  which mention `reduce` as one of several jobs they touched at the time,
  are left as-is — accurate history of what those ADRs did, not claims
  about current state.
- `.claude/skills/analyze-job-run/SKILL.md`: removed `job.reduce` from the
  documented `command_name()` values, the `reduce: download, placement`
  step-vocabulary entry, and the `reduce:` completion-summary format
  string — all three would otherwise describe a command that no longer
  exists.
- `CHANGELOG.md`: one new `[Unreleased]` bullet for this ADR. The existing
  ADR-0096/0099 entries are left untouched — an accurate record of what
  shipped at the time, not a live inventory of current commands.
- `README.md`: no change — confirmed it never mentions `reduce`.

## Consequences

- `pigeon` goes from 7 job types to 6; upload-capable jobs drop from 5 to 4
  (`email-sync`/`email-pull`/`pull-transform`/`deduplicate`).
- A user who wants `reduce`'s narrow behavior — auto-skip reproducible
  media/installer extensions during a bucket-to-bucket forward — loses
  that specific option. The accepted replacement is the manual
  `import` → `deduplicate` workflow, doing the valuable/reproducible
  judgment by hand instead of via a hardcoded extension table.
- `extension_of`/`SourceBucketInput` stay in their current shared
  locations (`core::data`, `shared_wizard.rs`) since both retain 2
  consumers after `reduce`'s removal.
- Metrics code needed no change beyond what's listed above:
  `record_phase`/`set_macro_phase` take a plain `&'static str` label, not a
  hardcoded job list, so nothing there referenced `reduce` by name.

## Out of scope

- Any replacement for `reduce`'s extension-based filtering — the decision
  is to do that judgment manually going forward, not to automate it
  differently.

## Verification

- `mise run ci` clean (fmt-check + lint + test, full suite minus the 6
  removed tests).
- `pigeon job run --help` no longer lists `reduce`; `pigeon job run reduce
  --help` fails as an unrecognized subcommand.
- Repo-wide grep for `"reduce"` as a job-name/command-name string turns up
  nothing left to update, excluding now-historical prose in
  `docs/adr/0096-0100`, `CLAUDE.md`, `CHANGELOG.md`, and two confirmed false
  positives (`docs/adr/0010`, `docs/adr/0071`) that use "reduce(d/s)" as
  plain English, unrelated to the job.
