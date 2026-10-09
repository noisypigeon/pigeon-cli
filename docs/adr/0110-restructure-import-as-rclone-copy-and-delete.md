# ADR-0110: restructure `import` as `pigeon job run rclone copy`/`rclone delete`

- **Author**: Willow Graysen ([@noisypigeon](https://github.com/noisypigeon)).
- **Date**: 2026-10-09.
- **Status**: Proposed.

## Context

`pigeon job run import` (ADR-0101, amended by ADR-0102/0106/0108) wraps a
single `rclone copy <source> <destination>` subprocess invocation.
`--source`/`--destination` are raw rclone `remote:path` strings passed
straight through to `rclone`'s argv -- deliberately *not* pigeon
`BucketConfig`/keyring aliases, unlike every other job's source/destination
flags. That distinction was a deliberate ADR-0101 design choice, but it was
only ever visible in a doc comment: `--source`/`--destination` read
identically to, say, `deduplicate`'s `--source-bucket`/`--destination-bucket`,
even though the two mean structurally different things (an opaque passthrough
string vs. a keyring-resolved alias). With only one rclone-backed job
existing, this ambiguity was latent. It stops being latent once a second
rclone-backed action needs a home under the same job family.

Separately, there is a real operational gap: buckets populated by `import`
(or by other jobs' uploads) sometimes need to be cleaned up or
decommissioned. Today that means hand-running `rclone purge` outside pigeon
entirely, losing this job family's `pigeon.jsonl` observability, metrics, and
ADR-0100 report-bucket upload trail for what is otherwise routine (if
destructive) housekeeping.

This ADR resolves both at once: nest `import` under a `pigeon job run rclone`
subcommand group with `copy`/`delete` actions, rename its path-passthrough
flags to `--source-path`/`--destination-path` (disambiguating them from every
other job's bucket-config-alias flags by name, not just by doc comment), and
add `delete` as a thin wrapper around `rclone purge`.

## Decision

### 1. Nested subcommand restructure

`JobType::Import { .. }` becomes `JobType::Rclone(RcloneArgs)`, where
`RcloneArgs::action: RcloneAction` is itself a `#[command(subcommand)]`
(`src/commands/job/cli.rs`) with two variants, `Copy { .. }` and
`Delete { .. }`. This mirrors the two-level `#[command(subcommand)]` nesting
already established by `KeyringArgs::command: KeyringCommands` ->
`AddArgs::kind: Option<AddKind>` (`src/commands/keyring/cli.rs`), with one
difference: `RcloneAction` is non-optional. Both actions are always given
explicitly on the command line -- this job's audience is scripted/CLI
operators, not an interactive first-run wizard picking an action.

```
pigeon job run rclone copy   --source-path <s> --destination-path <d> ...
pigeon job run rclone delete --source-path <s> ...
```

### 2. `--source`/`--destination` become `--source-path`/`--destination-path`

The rename makes explicit, at the flag-name level, what used to live only in
a doc comment: these are raw rclone path strings, not pigeon bucket-config
aliases. `copy` keeps every flag `import` had (`--source-path`,
`--destination-path`, `--local-output`, `--report-bucket`, `--transfers`,
`--checkers`, `--tpslimit`, `--yes`). `delete` takes `--source-path`,
`--local-output`, `--report-bucket`, `--checkers`, `--yes` -- no
`--destination-path` (purge has no destination), no `--transfers`/
`--tpslimit` (no file-transfer concurrency or rate concept applies to a
delete).

### 3. `delete` is `rclone purge <source-path>`, not `rclone delete`

rclone's own `delete` subcommand removes file *contents* but leaves empty
directory structure behind; `purge` recursively removes everything,
directories included -- the right semantics for "clean up this whole
prefix," and the one the user asked for by name. Its fixed/variable flag set:

```
rclone purge <source-path> --checkers <N> --fast-list \
  --retries 5 --low-level-retries 20 --stats 30s \
  --use-json-log --log-level INFO --log-file <path>
```

`--checkers` stays a per-run flag (default 16, same as `copy`) because purge
still enumerates objects before deleting them -- concurrency there is a real
dial. `--fast-list` is kept, not dropped with the other transfer-only flags:
it's a listing optimization, and purge's workload is dominated by listing
and deleting, not uploading, so it applies here too. `--retries`/
`--low-level-retries` and the logging flags are unchanged reliability/
observability scaffolding, reused verbatim. Dropped entirely: `--transfers`/
`--tpslimit` (no file content moves, so no transfer concurrency or rate
ceiling applies), `--buffer-size`/`--multi-thread-streams`/
`--multi-thread-cutoff` (these size and parallelize file-content transfer
buffers -- meaningless with nothing being read or written).

### 4. Per-action job naming, metrics, and log `step` field

`Observable::command_name()` matches one level deeper into `RcloneAction`:
`Copy { .. } => "job.rclone-copy"`, `Delete { .. } => "job.rclone-delete"`.
`job_name()` (unchanged, strips the `"job."` prefix) yields
`"rclone-copy"`/`"rclone-delete"` -- consistent with this codebase's existing
hyphenated job-name convention (`email-sync`, `pull-transform`,
`decrypt-files`, `email-pull`), and giving the two actions distinct
identities everywhere `job_name` feeds in: the `command` tracing span, the
`pigeon_job_phase_total`/`pigeon_job_macro_phase` metric labels, and the
ADR-0100 report-bucket upload prefix.

`import`'s existing metrics call, `record_phase_count("import", "transfer",
"transferred"/"failed", delta, None)`, splits into two call sites. `copy`
keeps the `"transfer"` phase and the existing `pigeon_upload_bytes_total`/
`pigeon_upload_outcomes_total` counters, relabeled `pigeon_job="rclone-copy"`.
`delete` calls `record_phase_count("rclone-delete", "delete",
"deleted"/"failed", delta, None)` and emits no bytes/outcomes metric at all
-- purge moves no bytes and isn't an "upload" in the sense those two metrics
model. `RcloneStats`/`RcloneLogSummary`/`TailDelta`
(`src/commands/job/rclone/rclone_log.rs`) gain a `deletes: u64` field, read
from rclone's JSON stats line's own `deletes` counter -- present on every
rclone JSON stats line regardless of operation, `0` for a pure copy.

This also reaches the log `step` field, not just the metric's `phase` label:
`RcloneLogTailer`'s own per-object-error `tracing::warn!` sites hardcode
`step = "transfer"` today (`rclone_log.rs`), and that code is shared by both
actions' tailers. `RcloneLogTailer` now carries its own `step: &'static str`
(`"transfer"` for copy, `"delete"` for delete), used consistently across its
own warn! sites and the one inside `worker.rs`'s delta-metrics emission for
collapsed-repeat error summaries -- so a `delete` run's errors are logged
`step = "delete"` ("rclone object deletion failed"), never a stale
`"transfer"` inherited from copy's code path.

### 5. Two `Job` implementors, not one with an action discriminant

`RcloneCopyJob`/`RcloneDeleteJob` (`src/commands/job/rclone/mod.rs`), each
with its own `Plan`/`Summary` type -- `RcloneDeleteSummary` has no `bytes`
field, since nothing is transferred. Their shapes diverge enough (no
destination/transfers/tpslimit/bytes on delete) that one struct with an
`enum Action` discriminant and `Option` fields for whichever half doesn't
apply would just reintroduce "ignore the fields that don't apply to this
branch" awkwardness, for no benefit: nothing holds a job value generically
across both actions, since dispatch (`src/commands/job/commands.rs`) already
branches on the action before either job type is constructed.

### 6. Destructive-action UX

`rclone delete`'s wizard prints an explicit warning before the existing
`--yes`/interactive confirm step: `"WARNING: this will recursively and
permanently delete everything under this path."` No new flag -- still gated
by the same `ConfirmInput` mechanism every other job already uses.

## Consequences

- **Breaking change, no migration shim** -- consistent with this codebase's
  established precedent for CLI reshapes (ADR-0017, ADR-0094, ADR-0096,
  ADR-0103). Every existing cron job, deployment script, or runbook invoking
  `pigeon job run import --source ... --destination ...` must be updated to
  `pigeon job run rclone copy --source-path ... --destination-path ...`
  before this ships; there is no transitional period where the old form
  still works.
- Any Grafana/Cockpit dashboard panel or alert rule querying
  `pigeon_job="import"` (on `pigeon_job_phase_total`, `pigeon_job_macro_phase`,
  `pigeon_upload_bytes_total`, `pigeon_upload_outcomes_total`) must move to
  `pigeon_job="rclone-copy"` -- a new, distinct label value, not a
  relabel-in-place; old and new series will not coexist.
- `rclone delete`/`rclone purge` is destructive and irreversible: no
  dry-run, no trash/undo, no confirmation beyond the existing `--yes`/
  interactive step. An operator running it non-interactively with `--yes`
  against a wrong `--source-path` has no safety net beyond getting the path
  right.
- `pigeon`'s `import` job, as a name, ceases to exist in any user-facing
  surface; `rclone-copy`/`rclone-delete` fully replace it.

## Out of scope

- A `--dry-run` flag for `delete` -- if this proves needed operationally,
  it's a follow-up ADR, not bundled here.
- Any confirmation mechanism beyond the existing `--yes`/interactive
  `ConfirmInput` -- no "type the path to confirm" style second gate, no
  `--force` flag duplicating what `--yes` already does.
- Deleting a subset by glob/pattern -- `delete` always purges everything
  recursively under `--source-path`; partial deletion means running `rclone`
  directly outside pigeon, or a later ADR if the need materializes.
- A `--tpslimit` flag for `delete` -- if a provider turns out to rate-limit
  delete/list operations the way ADR-0106 found for transfers, that's a
  future amendment, same shape as ADR-0106 -> ADR-0108's relationship to
  ADR-0101.
- Any path-prefix allowlist/denylist or other guard against purging an
  unintended path -- the operator is fully trusted with the path they
  supply, the same trust model ADR-0101 already established for `copy`.
- Retroactively relabeling historical `pigeon_job="import"` metrics or log
  data -- out of scope; only new runs use the new names.

## Amends

ADR-0101 (original `import` job -- superseded in its CLI shape and naming;
the "raw passthrough string" design survives), ADR-0102 (live metrics
polling mechanism preserved, generalized per-action), ADR-0106 (tuned
`copy` defaults preserved unchanged; `delete`'s `--checkers` reuses the same
default), and ADR-0108 (CLI-flag exposure preserved for `copy`; deliberately
not offered on `delete`). Each of those four ADR files gets a short
"Amendment" section appended, pointing forward to this one, matching the
pattern ADR-0101 itself already carries from ADR-0106/ADR-0108.

## Verification

- `mise run ci` clean (fmt-check + lint + test).
- `rclone_log.rs` unit tests cover a `deletes`-bearing stats line tracked
  distinctly from `transfers`.
- `worker.rs`'s existing end-to-end `rclone copy` tests pass under their
  renamed call (`run_copy_job`); a new end-to-end `rclone purge` test
  (skipped if `rclone` isn't on `PATH`, same as today) confirms a tempdir
  tree is fully removed and the reported `deleted` count is sane.
- `tests/cli.rs`: renamed/new help-text and fail-fast tests confirm `rclone
  copy --help` shows `--source-path`/`--destination-path`/`--transfers`/
  `--checkers`/`--tpslimit`, and `rclone delete --help` shows
  `--source-path`/`--checkers` but **not** `--destination-path`/
  `--transfers`/`--tpslimit`.
- Manual: run both `rclone copy` and `rclone delete` against a real
  `rclone.conf`-backed remote, confirming `pigeon.jsonl` carries
  correctly-labeled `rclone-copy`/`rclone-delete` entries, the report-bucket
  upload succeeds, and the delete run's uploaded report shows a sane
  `deleted` count.
