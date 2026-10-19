# CLI-0113: refactor `deduplicate`'s bucket actions to use `rclone`

- **Author**: Willow Graysen ([@noisypigeon](https://github.com/noisypigeon)).
- **Date**: 2026-10-10.
- **Status**: Proposed -- **superseded by CLI-0133** before any code
  landed. The intent below (rclone-backed source/destination, three-phase
  pipeline, `--upload-only` and the preview table kept) stands and is
  carried forward; the mechanism it names (`rclone_transfer::run_rclone_copy`)
  was deleted by CLI-0132, its dead-code Consequence was invalidated by
  CLI-0131's removal of `pull_transform`, its `--report-bucket` row was
  superseded by CLI-0132, and its strict pull-then-process barrier would
  have regressed throughput. No code from this ADR is in `main`.

## Context

`pigeon job run deduplicate` (CLI-0082, amended by CLI-0089/0090/0091/0095/
0098/0099/0109/0111) talks to buckets through exactly three call sites --
`list_objects`, `download_object_to_file`, `upload_if_changed` -- all
funneling through `src/commands/keyring/bucket/client.rs`'s direct `minio`
crate usage, reached via the shared `src/commands/job/download.rs`/
`upload.rs` wrappers. Every dedup-specific concern -- zip expansion, SHA-256
content hashing, placement, checkpointing, the merge report -- is pure
local-disk work that never touches a bucket directly; the S3-touching
surface is already narrow and already isolated behind those three
functions.

Separately, this codebase has twice built and reused a different
bucket-interaction mechanism: `pigeon job run rclone copy`/`rclone delete`
(CLI-0101, restructured by CLI-0110) shells out to the `rclone` binary with
raw `remote:path` strings -- reaching any rclone remote, not only pigeon's
own S3-compatible `BucketConfig` buckets -- with live JSON-log polling for
metrics, hoisted into a shared `rclone_transfer::run_rclone_copy` helper.
CLI-0112's `transform` job then combined that exact rclone data-movement
mechanism with `deduplicate`'s own local-processing/checkpoint/placement
architecture: pull via `rclone copy` into a local staging tree, do local
work, push the result via `rclone copy`. `transform` is the direct
architectural template for this refactor.

Applying that template to `deduplicate` does two things at once: lets a
dedup run reach arbitrary rclone remotes (not only pigeon's
keyring-managed S3-compatible buckets) for its source(s) and destination,
and retires `deduplicate`'s own bespoke per-object download/upload plumbing
in favor of the already-proven, already-shared rclone pull/push legs --
while leaving the dedup logic itself (hashing, zip expansion, placement,
report) completely untouched.

Three design questions were resolved up front, each following the more
conservative/safer of two options rather than defaulting to `transform`'s
precedent verbatim:

1. **Addressing**: source/destination become raw rclone paths, not
   `BucketConfig` aliases translated to rclone remotes under the hood --
   avoids reintroducing the pigeon-to-rclone credential translation
   CLI-0101 deliberately avoided.
2. **`--upload-only`**: kept, rather than dropped the way `transform`
   dropped its equivalent -- preserves CLI-0089's original guarantee that
   resuming a crashed upload needs no access to the source remote at all.
3. **Pre-run preview**: kept, via `rclone lsjson --recursive` instead of
   `list_objects` -- preserves the existing confirm-before-pulling UX
   rather than letting an operator discover what's there only from pull
   progress/logs.

## Decision

### 1. Command surface

| Flag | Before | After |
|---|---|---|
| Source | `--source-bucket` (repeatable, `BucketConfig` alias, CLI-0109) | `--source-path` (repeatable, raw rclone `remote:path` string) |
| Destination | `--destination-bucket` (optional, `BucketConfig` alias) | `--destination-path` (optional, raw rclone `remote:path` string) |
| Transfer tuning | none (per-object concurrency only) | `--transfers`/`--checkers`/`--tpslimit` (rclone/transform convention, defaults 8/16/unset) |
| Upload concurrency | `--upload-concurrency` (CLI-0091) | removed -- meaningless once push is a bulk `rclone copy`, not a per-file task queue |
| Report bucket | `--report-bucket` (CLI-0100, `BucketConfig` alias) | unchanged -- orthogonal to this refactor, same as it already is for `rclone`/`transform` |
| Everything else | `--local-output`, `--concurrency`, `--upload-only`, `--yes` | unchanged |

`--source-path`/`--destination-path` are opaque strings passed straight
into `rclone`'s argv, exactly like `rclone copy`/`transform`'s flags of the
same name -- never resolved against the keyring. `deduplicate` gains the
same hard runtime dependency on the `rclone` binary
(`check_rclone_available()`, reused unchanged) that `rclone`/`transform`
already have, plus an externally-provisioned `rclone.conf` for any
non-local remote -- pigeon takes zero responsibility for provisioning or
validating it, same stance CLI-0101 already established.

### 2. Three-phase pipeline

```
Phase A (pull)        rclone lsjson --recursive <source-path>   (preview, per source)
                       rclone copy <source-path> <local-output>/.staging/source/<n>/ ...
Phase B (local dedup)  local: zip-expand -> hash -> ContentIndex check -> place -> checkpoint
Phase C (push)         rclone copy <local-output>/result <destination-path> ...
```

**Phase A (pull)** -- skipped entirely under `--upload-only`. Otherwise,
for each `--source-path`:

- Preview: `rclone lsjson --recursive <source-path>`, a one-shot JSON-array
  call (not the progress-polling `RcloneLogTailer` -- there is no ongoing
  transfer to poll), parsed into the same `TypeSummary` (extension ->
  count/bytes) table the wizard already shows pre-confirm today.
- Pull: `rclone copy <source-path> <local-output>/.staging/source/<n>/
  --transfers --checkers --tpslimit` via `rclone_transfer::run_rclone_copy`
  -- `deduplicate` becomes this helper's 4th real consumer, after `rclone
  copy` and `transform`'s pull/push, confirming the CLI-0082-style "hoist
  on the third consumer" bar stays met as more jobs adopt it.
  `job_name="deduplicate"`, `phase_label="pull"`. Multiple `--source-path`
  values land in distinct numbered subdirectories under `.staging/source/`
  so provenance survives into Phase B's report/checkpoint without needing
  any bucket-alias concept.

**Phase B (local dedup)** -- same logic as today, new input shape: walks
the already-pulled local tree instead of driving a per-object
download-then-hash queue. For each local file: a zip is expanded via
`pull_transform::archive::expand_to_dir` (unchanged, members recurse into
the same walk); anything else is hashed via `download::sha256_file`
(unchanged, still a streamed 64 KiB-chunk SHA-256, still never loads a
whole file into memory). `ContentIndex` check/commit and `place_and_report`
carry over unmodified. `deduplicate-report.txt`'s one schema change: the
CLI-0099 `source_bucket_alias` column becomes `source_path` -- the raw
rclone string the kept copy's pull subdirectory maps back to.
`--concurrency` now bounds a purely CPU-bound pool with zero network calls
in this phase at all -- the same framing `transform`'s Phase B already
uses. The `.staging/.processed` checkpoint's compound key changes from
`(bucket_alias, key)` (CLI-0109) to `(source_path, relative_path)` -- same
shape, new identity, since there is no more bucket alias to key on.

**Phase C (push)** -- runs whenever `--destination-path` is given, or
always under `--upload-only` (where it remains mandatory, as today).
`rclone copy <local-output>/result <destination-path> --transfers
--checkers --tpslimit` via the same `run_rclone_copy`, `phase_label="push"`,
`emit_upload_metrics=true`.

### 3. `--upload-only` preserved

Preflight is unchanged (`.staging/.processed` must exist, `result/` must
be non-empty); `--destination-path` becomes mandatory in this mode exactly
as `--destination-bucket` was before. This mode skips Phase A entirely and
runs Phase C only -- preserving CLI-0089's original guarantee that resuming
a crashed/interrupted run needs no access to the source remote (and, by
extension, no `rclone.conf` entry for it) at all. This is a deliberate
departure from `transform`'s CLI-0112 §6 precedent, which dropped the
equivalent flag in favor of full-rerun resumability -- that trade only
makes sense when every phase is cheaply re-enterable, and `deduplicate`'s
pull phase (a full recursive remote listing/copy, potentially spanning
several source paths at real scale) is not cheap enough to default to
redoing it on every resumed run.

### 4. Metrics

`pigeon_job="deduplicate"` is unchanged (no job rename, unlike
`import`->`rclone-copy`). New `phase="pull"|"push"` labels on
`pigeon_job_phase_total`, emitted via `run_rclone_copy`'s existing
delta-polling, replace whatever phase labels the old per-object
download/upload-phase metrics used for this job.
`pigeon_upload_bytes_total`/`pigeon_upload_outcomes_total` are emitted for
Phase C only, matching `transform`'s Phase A/C split (a pull is not an
upload). Per CLI-0102's already-accepted, already-tracked gap,
`pigeon_upload_attempts_total`/`pigeon_upload_duration_seconds` -- genuinely
per-file measurements rclone's aggregate JSON stats can't produce without
per-object-level logging this job's fixed flag set doesn't enable -- are
now also unavailable for `deduplicate`'s push, same as every other
rclone-backed job; this is noted explicitly here rather than silently
regressing. `set_macro_phase("deduplicate", false)` at Phase B start,
`true` at Phase C start, same convention every other job already follows.

## Consequences

- **Breaking change, no migration shim** -- consistent with this
  codebase's established precedent (CLI-0017, CLI-0094, CLI-0096,
  CLI-0103, CLI-0110). `--source-bucket`/`--destination-bucket`/
  `--upload-concurrency` disappear from the CLI; any cron job, deployment
  script, or runbook invoking them must switch to
  `--source-path`/`--destination-path` before this ships.
- Existing `.staging/.processed`/`.content-hashes` files from a
  pre-refactor run are keyed on bucket-alias+key, structurally
  incompatible with the new source-path+relative-path keys -- a resumed
  run needs a fresh `--local-output` directory, not a reused one. This ADR
  does not remediate or convert already-written checkpoint state.
- `src/commands/job/download.rs`'s `download_with_retry`/
  `DownloadAnnounce`/`check_disk_space` drop to one remaining caller
  (`pull_transform`); `src/commands/job/upload.rs`'s
  `pending_upload_tasks`/`run_upload_phase`/`UploadedIndex` drop to three
  (`pull_transform`, `email_sync`, `email_pull`). Neither module becomes
  dead code -- no removal follows from this ADR.
- The `.uploaded` resume-index file (shared `upload.rs` infra) no longer
  applies to `deduplicate`'s push leg -- superseded by `rclone copy`'s own
  incremental size/modtime skip, the same mechanism `rclone`/`transform`
  already rely on for idempotent reruns.
- `deduplicate` can now dedup from or to any rclone remote, not only
  pigeon's keyring-managed S3-compatible buckets -- a real capability
  expansion, but pigeon's own credential management no longer covers
  dedup's main data legs (only `--report-bucket` still goes through the
  keyring).
- Any Grafana/Cockpit panel or alert rule keyed on `deduplicate`'s old
  phase-metric label values must be updated to the new
  `phase="pull"|"push"` values -- old and new label values will not
  coexist for this job, the same kind of break CLI-0110 already flagged
  for `import`'s metrics.

## Out of scope

- Any `BucketConfig`-to-rclone-remote auto-translation -- explicitly
  rejected; `--source-path`/`--destination-path` are raw passthrough
  strings only, matching `rclone`/`transform`'s existing convention.
- Dropping `--upload-only` or the pre-run preview table -- both explicitly
  kept, per the decisions in Context.
- Adding client-side encryption support to `deduplicate` -- it has never
  offered this, and this refactor doesn't change that.
- Migrating or converting old `.staging` checkpoint files written by the
  pre-refactor `deduplicate` to the new key format.
- Touching `pull_transform`'s still-minio-based download/upload path, or
  any other job type's bucket interactions -- this ADR is scoped to
  `deduplicate` alone.
- Implementation itself -- this ADR documents the design; a follow-up PR
  implements it.

## Amends

CLI-0082 (original `deduplicate` architecture, amended by this ADR's
bucket-interaction legs), CLI-0089 (streaming-upload-via-`minio` mechanism,
no longer used by `deduplicate`'s push leg specifically), CLI-0091
(`--upload-concurrency`, removed for `deduplicate`), CLI-0099 (merge
report's `source_bucket_alias` column, renamed `source_path`), CLI-0109
(multi-source-bucket support, generalized from keyring aliases to raw
paths). Draws on, without amending, CLI-0101/CLI-0110 (the `rclone`
job's subprocess/flag conventions this reuses) and CLI-0112 (`transform`,
the direct architectural template: rclone pull -> local work -> rclone
push).

## Verification

- `mise run ci` clean (fmt-check + lint + test).
- New unit tests: the `rclone lsjson` preview-parser builds the correct
  `TypeSummary` from a sample JSON array; the new checkpoint key format
  round-trips `(source_path, relative_path)` correctly, including the
  multi-source-path case; `deduplicate-report.txt`'s `source_path` column
  reflects the correct source for a file pulled from the second of two
  `--source-path` values.
- `rclone_transfer::run_rclone_copy`'s existing tests (written for `rclone
  copy`/`transform`) pass unchanged under `deduplicate` as a 4th consumer
  -- regression check that the hoisted helper really is generic.
- `tests/cli.rs`: `deduplicate --help` shows `--source-path` (repeatable),
  `--destination-path`, `--transfers`, `--checkers`, `--tpslimit`,
  `--upload-only`; no longer shows `--source-bucket`, `--destination-bucket`,
  `--upload-concurrency`. Missing `rclone` on `PATH` fails fast with a
  clear error before any prompt, same as `rclone copy`/`transform` today.
- Manual: run `deduplicate` end-to-end against plain local directories
  (rclone treats a bare path as implicitly local, no `rclone.conf` needed
  -- same precedent `transform`'s manual verification used) with a mix of
  plain files and a zip containing a duplicate, confirming placement,
  report, and checkpoint are all correct; rerun the same command and
  confirm already-processed files are skipped via the checkpoint; run
  again with `--upload-only` and confirm the push-only path succeeds
  without re-pulling anything.

## Amendment (2026-10-12): superseded by CLI-0133

CLI-0133 replaces this ADR as the live design for migrating `deduplicate`'s
data operations to `rclone`. It carries forward every decision recorded above
-- raw `--source-path`/`--destination-path` rather than keyring aliases,
`--upload-only` kept, the `lsjson` preview table kept -- and changes four
things this ADR could not have known:

1. The mechanism is now `core::rclone`'s `RcloneCommand` trait and its
   `Copy`/`Lsjson` implementors (CLI-0132), not
   `rclone_transfer::run_rclone_copy`, which no longer exists.
2. This ADR's Consequences state that `download_with_retry`/`DownloadAnnounce`
   "drop to one remaining caller (`pull_transform`)" and that no removal
   follows. CLI-0131 deleted `pull_transform`, so they drop to **zero** and
   `clippy -D warnings` forces their deletion.
3. The `--report-bucket` row in §1's table is stale -- CLI-0132 renamed it to
   `--report-path` for this job.
4. §2's strict Phase A -> Phase B barrier is replaced by consuming
   `rclone::Copy`'s `on_copied` channel, because a hard barrier would discard
   the download/process overlap the current worker pool already achieves and
   make a run slower than it is today.

CLI-0133 also settles two questions this ADR left open: what the per-item
`source_bucket` metric label carries once aliases are gone, and the full extent
of CLI-0109's bucket-identity threading that has to migrate with it.
