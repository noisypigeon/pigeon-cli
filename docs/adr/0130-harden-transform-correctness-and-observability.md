# ADR-0130: harden `pigeon job run transform`'s correctness and observability

- **Author**: Willow Graysen ([@noisypigeon](https://github.com/noisypigeon)).
- **Date**: 2026-10-12.
- **Status**: Accepted.

## Context

`pigeon` ships two overlapping media-transform jobs. `pull-transform`
(ADR-0074, amended by ADR-0075/0076/0077) is the older one: native S3 via the
`minio` crate, keyring bucket-config aliases, a whole-bucket sweep, zip
expansion, date extraction, SHA-256 content dedup, and optional client-side
encryption. `transform` (ADR-0112, reworked by ADR-0116/0124/0127/0129) is the
newer architecture: all storage I/O shells out to `rclone`, addressing is a raw
`remote:path`, output names are deterministic so dedup is unnecessary by
construction, and the pipeline is per-file with immediate push and local GC.

`transform` is the direction this codebase is moving — ADR-0113 planned to
migrate `deduplicate` onto its pull/local-work/push template. ADR-0131 removes
`pull-transform` outright. Before doing that, an audit comparing the two found
`transform` is materially *less* hardened than the job it supersedes, including
four real defects. This ADR fixes those and closes the observability gap. It
deliberately does not port `pull-transform`'s feature set (zip expansion, date
organization, content dedup, encryption) — those are either covered by
`deduplicate` or intentionally absent per ADR-0112.

The four defects, in descending severity:

1. **A failed download silently vanishes.** `run_transform_job` reads only
   `pull_summary.transferred` (`worker.rs:803`). `RcloneLogSummary.errors`
   (`rclone_log.rs:45`) is never read anywhere in the job. A source file
   `rclone` fails to pull never appears in `outcomes`, never increments a
   failure count, and never affects the exit code — the run reports success
   having silently skipped it.
2. **macOS AppleDouble stubs can abort an entire run.**
   `destination::list_source_relative_paths` builds `--include '*.{ext}'
   --ignore-case`, which matches AppleDouble resource forks (`._IMG_0001.heic`).
   These 82-byte stubs get pulled and fed to `ffmpeg`, which fails with
   `"Invalid data found when processing input"` — a string in
   `media::NON_RETRYABLE_SIGNATURES`. On a macOS-originated source tree where
   `._*` entries cluster early in the listing, all of the first
   `CIRCUIT_BREAKER_SAMPLE_SIZE` (20) completions can be stubs, tripping
   ADR-0121's circuit breaker and aborting the run with a message that
   misleadingly blames the host's ffmpeg build.
3. **`set_macro_phase` violates its own documented invariant.**
   `worker.rs:704` flips `pigeon_job_macro_phase` to `1` on the first
   successful push, while pull and transcode are still running. The gauge's
   doc comment grounds its one-way guarantee in ADR-0019 ("upload never starts
   until all local work is done"), a premise ADR-0116's per-file pipeline
   structurally violates. The metric is a false signal.
4. **No disk-space preflight.** `download::check_disk_space` (ADR-0076) has
   three production call sites, none in `transform`. ADR-0124's GC bounds
   steady-state usage but not the transient peak, and `remove_local_copy` is
   best-effort, so repeated removal failures accumulate sources with nothing
   watching free space.

Separately, `transform/` emits **zero** `tracing::info!` lines. Its final
summary goes through `report_upload::say`, which is `println!` plus a file
write, not `tracing`. So the ADR-0073 `pigeon.jsonl` log that the
`analyze-job-run` skill reads contains no `transform` run totals at all, and
the job is absent from the shared `pigeon_upload_*` dashboards.

## Decision

### 1. Reconcile the pull phase against its own expected set

Rather than merely reading `pull_summary.errors` (an aggregate rclone reports
without per-object detail unless `-v` logging this job doesn't enable — the
same limit ADR-0102 hit for `import`), reconcile against what the job already
knows it asked for.

`files_needing_pull` computes the exact set of relative paths this run intends
to download. Every path that enters the pipeline from either dispatch source
passes through `enqueue`, which records it in the `dispatched` set. So after
the loop drains, any intended path not in `dispatched` was never delivered —
by definition a pull failure, at per-file fidelity, with no new rclone
plumbing.

Each such path becomes a real `FileOutcome` with `Outcome::Failed` and
`FailureStage::Pull`, so it lands in the report, the failure breakdown, and
the exit code. `pull_summary.errors` is retained as a cross-check: a
disagreement between the two counts emits a `tracing::warn!` rather than being
silently reconciled away, since the two measure subtly different things
(rclone counts transfer errors; this counts undelivered files).

Reconciliation runs *after* the circuit-breaker check, so a tripped breaker —
which deliberately stops dispatch and therefore leaves most paths undispatched
— returns its own error instead of reporting hundreds of spurious pull
failures.

### 2. Skip AppleDouble sidecars before they reach ffmpeg

Filter them at both choke points:

- `files_needing_pull` excludes them, so they are never downloaded at all.
- `enqueue` excludes them, covering the initial on-disk scan and the live
  rclone tail — the single point every dispatch path passes through, so
  nothing can slip past.

Counted as `skipped_apple_double` in the summary (mirroring
`pull_transform`'s own ADR-0107 field name), explicitly *not* as a failure:
these files carry no user data and their absence is correct.

Rather than write a second copy of the predicate, hoist
`is_apple_double_basename` from `pull_transform/archive.rs` into
`core::data`, which already owns this codebase's filename predicates
(`sanitize_filename`, `extension_of`, `after_zip_separator`). `archive.rs`'s
own `is_apple_metadata_entry` repoints to it. This is its second consumer,
matching `download.rs`'s own stated "second consumer" hoist reasoning, and it
means ADR-0131 does not have to move this function as part of a deletion.

### 3. Stop emitting `pigeon_job_macro_phase` for `transform`

Remove both call sites and the `macro_phase_is_uploading` latch. `transform`
has no one-way local-work→upload transition to report: ADR-0116's pipeline
pushes the first file while the last is still downloading. `set_macro_phase`'s
doc comment gains a note that pipelined jobs deliberately do not emit it.

Rejected alternative: redefining the gauge to mean "has any upload started".
That would silently change what the metric means for the four jobs that emit
it correctly, to accommodate the one job for which it was never meaningful.
Per-phase counters (`pigeon_job_phase_total`, which `transform` already emits
for `pull`/`transcode`/`push`) are the right instrument for a pipelined job.

### 4. Disk-space preflight at two points

- Once before the pull subprocess spawns, against `source/`, failing the run
  fast (nothing has been downloaded yet, so there is nothing to salvage).
- Per-file in `process_one` before transcoding, against the scratch directory.
  A 4K video's scratch output is multi-GB, and this is the point where the
  run's disk high-water mark is reached. Fails only that one file, matching
  `check_disk_space`'s documented "fails only the one item calling this"
  contract, and categorized as a `Transcode`-stage failure.

Reuses `download::check_disk_space` and `MIN_FREE_DISK_BYTES` unchanged. This
confirms `download.rs` keeps two consumers (`deduplicate` and `transform`)
after ADR-0131, which is why ADR-0131 does not un-hoist it.

### 5. Per-stage failure categorization

`process_one` currently returns `Result<PathBuf, String>`, so which stage
failed survives only as prose inside `FileOutcome.detail`. Introduce
`FailureStage { Pull, Transcode, Place, Push }`; `process_one` returns
`Result<PathBuf, (FailureStage, String)>`, and `TransformSummary` gains a
counted `FailureBreakdown` with one field per stage — the same shape every
other job's summary already has, and the field names
`record_phase`'s doc comment already expects jobs to reuse.

This also sharpens the circuit breaker: it now requires
`stage == FailureStage::Transcode` *and* a non-retryable error signature,
where before any error string that happened to match could count toward
tripping it.

### 6. Observability parity

- Phase-boundary `tracing::info!` lines bracketing the pull phase, plus a
  structured end-of-run totals line carrying the full breakdown — mirroring
  `pull_transform`'s own four phase-boundary lines, so `pigeon.jsonl` finally
  contains `transform` run totals.
- `skipped_already_done` count in the summary. `files_needing_pull` discards
  already-done paths silently, so a run that correctly skipped 500k files was
  indistinguishable in the report from one that found nothing to do.
- `pigeon_upload_bytes_total` / `pigeon_upload_outcomes_total` emitted from
  `push::push_one`, so `transform` appears on the shared upload dashboards
  alongside every other upload-capable job. `destination_bucket` uses
  `metrics::NO_BUCKET`, following ADR-0102's precedent for a job whose
  destination is a raw rclone connection string rather than a `BucketConfig`
  alias — an unbounded-cardinality label otherwise.
  `pigeon_upload_attempts_total`/`_duration_seconds` stay unimplemented for
  the same genuine reason ADR-0102 recorded for `import`.
- `#[tracing::instrument]` on `process_one` for per-file log correlation.

### 7. Report and wizard summary

`write_report`'s trailing summary line and the wizard's printed narration both
gain the breakdown, the skipped-already-done count, and the AppleDouble count.
The per-file table gains a `stage` column so a failed row says which stage
failed without parsing `detail`.

## Out of scope

- **Fallback-to-original on a failed transcode.** `pull-transform` keeps the
  original file when recode verification fails. Porting it would break
  `transform`'s contract that every destination object is a verified
  transcode, and would push e.g. a PNG under a `.jpg` name. Filed as an
  `out-of-scope` issue per ADR-0031. ([#62](https://github.com/noisypigeon/pigeon-cli/issues/62))
- **`destination.rs`'s "directory not found" → empty-listing mapping.** A
  transient destination-side error whose stderr happens to contain that
  substring makes the whole destination look empty, re-pulling and re-pushing
  everything. The module doc already flags the fragility. Real, but a distinct
  concern from this ADR's four defects. Filed as an `out-of-scope` issue. ([#63](https://github.com/noisypigeon/pigeon-cli/issues/63))
- Progress bar and large-operation announcements (`pull-transform`'s ADR-0075
  equivalent). A long `transform` run still prints nothing between the
  confirmation and the summary. Deliberately deferred.
- Any of `pull-transform`'s features: zip expansion, date extraction/
  organization, content dedup, multi-format output menus, client-side
  encryption.

## Consequences

- A `transform` run can no longer silently lose a file to a failed download;
  such a file is reported and makes the exit code non-zero. Runs that
  previously "succeeded" against a flaky source may now correctly fail.
- A macOS-originated source tree no longer risks aborting the whole run, and
  its `._*` stubs are neither downloaded nor transcoded.
- `pigeon_job_macro_phase` no longer has a `pigeon_job="transform"` series.
  Any dashboard panel filtering on it loses that one job — correct, since the
  series was reporting a transition that never happened as described.
- `TransformSummary`'s shape changes (`failed: bool` → `FailureBreakdown`,
  plus three new counts). Internal only; `transform-report.txt` gains a column
  and richer totals, which is a format change for anyone parsing it.
- `core::data` gains `is_apple_double_basename` with two consumers, so
  ADR-0131's deletion does not need to relocate it.

## Verification

- `mise run ci` clean.
- Unit test: a path in the intended pull set that is never dispatched is
  reported as a `Pull`-stage failure and sets the run's failure state.
- Unit test: a `._*` path is excluded from both `files_needing_pull` and
  `enqueue`, and is counted as skipped rather than failed.
- Unit test: `is_apple_double_basename` in its new `core::data` home.
- Unit test: `FailureBreakdown` tallies each stage from a mixed outcome set.
- Unit test: reconciliation is skipped when the circuit breaker tripped.
- Manual: `pigeon job run transform --help`; a real run against a source tree
  containing a `._*` stub and a genuinely corrupt file, confirming
  `pigeon.jsonl` carries an end-of-run totals line and the run does not abort.

## Amendment (2026-10-12): push-leg metrics move to the shared runner (ADR-0132)

ADR-0132 preserves the `pigeon_upload_*` emission this ADR added to
`transform`'s push leg, and the decision not to emit `pigeon_job_macro_phase`
for this job at all -- but the emission site moves out of `push.rs` and into
`core::rclone`'s shared runner, so every rclone command's metrics come from
one place rather than from whichever caller remembered to add them. The
`is_apple_double_basename` filtering, per-file pull reconciliation, and
disk-space preflight this ADR added are unaffected. Separately, `transform`'s
`--report-bucket` becomes `--report-path`, leaving this job with no keyring
dependency at all.

## Amendment (2026-10-13): the undelivered-file message was false for one class (ADR-0144)

The per-file pull reconciliation this ADR introduced is correct, and caught
two genuinely undelivered files in a real run. Its *message* was not: for a
source key that differs from another only by letter case, "rclone never
reported it as copied" is simply untrue -- rclone reports exactly that case
as `Duplicate object found in source - ignoring` at **`notice`** level,
which this job never reads, while leaving its aggregate error count at zero.

That is also why this ADR's own `rclone_errors != failures.pull` cross-check
fired on that run (`rclone_errors=0, undelivered_files=2`): the mismatch it
exists to surface was real, and the surfacing worked.

ADR-0144 detects such collisions up front, warns with every colliding key
named before the pull starts, and moves the detail into a pure
`undelivered_detail` function with an accurate message per class. The
pull-phase `tracing::info!` line this ADR added gains a
`case_colliding_keys` count. The reconciliation mechanism itself is
unchanged.

## Amendment (2026-10-14): a case collision no longer produces an undelivered file (ADR-0152)

This ADR's pull reconciliation -- diffing the intended pull set against the
dispatched set -- is unchanged and still correct. What changes is that one of
its two known causes goes away: ADR-0152 pulls every case-colliding key
individually, so such a key is no longer dropped by the bulk pull and no
longer shows up as a `Pull` failure.

The practical effect is that this ADR's `rclone_errors` vs.
`undelivered_files` cross-check should go quiet for that class, and ADR-0151
§6's "fully accounted for by case-colliding source keys" wording becomes a
fallback rather than the common case. A colliding key can still fail -- its
own `copyto` can fail like any other -- and then it is reported with rclone's
actual error rather than with prose about duplicate handling that no longer
applies.
