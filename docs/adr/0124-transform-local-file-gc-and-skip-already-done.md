# ADR-0124: `transform` garbage-collects local files after push, and skips already-done files before downloading

- **Author**: Willow Graysen ([@noisypigeon](https://github.com/noisypigeon)).
- **Date**: 2026-10-11.
- **Status**: Proposed.

## Context

`pigeon job run transform` (ADR-0112, reworked by ADR-0116, resume/observability fixed by
ADR-0120, hardened by ADR-0121, extended to video by ADR-0122) pulls files from an arbitrary
`rclone` remote into `<local_output>/source/`, transcodes them into
`<local_output>/result/`, and pushes each one individually to `--destination-path` via
`rclone copyto`. Nothing in this pipeline ever deletes a local file: both `source/` (every
original download) and `result/` (every transcoded output) accumulate for the entire run and
are only ever freed when the job's ephemeral VM itself is torn down. For a several-hundred-GB
source prefix, this means the local volume has to be provisioned for roughly *source size +
result size* resident simultaneously -- a real operational cost reported directly by this
job's user.

A second, related gap sits in ADR-0120's own "Accepted limitation": Phase A's bulk
`rclone copy` has no way to exclude files already pushed to the destination in a prior run --
it unconditionally re-downloads them every time. ADR-0120 reasoned that a pull-side
`--exclude` filter wasn't feasible because "the destination's hash-based naming is one-way."
That framing turns out to be overly pessimistic: `placement::compute_destination_name
(source_path, relative_path)` is a pure, *forward* function -- it needs no file content, only
the relative path, which a lightweight `rclone lsjson` listing of the source tree already
provides with no bytes transferred. So the destination-name check `enqueue` already performs
*after* download can just as well run *before* it, against a remote listing -- the inversion
ADR-0120 considered infeasible was never actually necessary.

## Decision

### 1. Delete local source + result files immediately after a successful push

In `src/commands/job/transform/worker.rs::process_one`, immediately after `push::push_one`
returns `Ok` and before returning `Ok(final_path)` -- inside the per-file concurrent task,
which is safe without any synchronization since each task only ever touches its own two
distinct paths -- a new best-effort helper, `remove_local_copy(path: &Path)`, removes both
the original downloaded source file (`pending_file.absolute_path`, under
`<local_output>/source/`) and the placed result file (`final_path`, under
`<local_output>/result/`). A removal failure (e.g. a permissions issue) is logged via
`tracing::warn!` but never turns an already-successful push into a failed `Outcome` --
cleanup is strictly best-effort, mirroring this codebase's existing best-effort precedent
(ADR-0068's logout handling).

The scratch file needs no equivalent handling: `placement::place_one` already consumes it via
`fs::rename` into `result/`, so nothing is left at the scratch path once placement succeeds.

On any failure path (transcode failure, push-retry exhaustion, the ADR-0121 circuit breaker),
nothing is deleted -- source (and, if somehow placed, the result file) stays on disk for
diagnosis, unchanged from today's behavior.

No new CLI flag. `transform` has no `--upload-only`/local-state-dependent resume mechanism
(ADR-0112 §6/Consequences: every phase is independently rerun-safe via the remote
destination listing, not local checkpointing) -- unlike `deduplicate`/`pull_transform`, which
deliberately retain `result/` to support `--upload-only`. There is nothing for a retention
flag to preserve here, so none is added.

### 2. Skip already-done files before downloading, not after

Before Phase A's bulk `rclone copy` is spawned -- after `destination::list_existing_filenames`
is fetched, same as today -- `run_transform_job` now:

1. Lists the *source* tree via a new `destination::list_source_relative_paths(source_path,
   extension)`: a recursive, metadata-only `rclone lsjson --recursive --files-only --include
   '*.<ext>' --ignore-case <source_path>` (the same extension filter Phase A's pull already
   applies), sharing `list_existing_filenames`'s retry-with-backoff shape and
   "directory not found" → empty-list handling (the module's constants are renamed from
   `DESTINATION_LISTING_*` to `LSJSON_*` since they now back both listings).
2. A new pure function, `worker::files_needing_pull`, filters that list down to relative
   paths whose `placement::compute_destination_name(source_path, relative_path,
   output_extension)` is **not** already present in `existing_destination_filenames`.
3. The excluded (not-yet-done) relative paths are written newline-delimited to
   `<local_output>/.staging/pull-files-from.txt` (`worker::write_files_from`) and passed to
   `rclone_transfer::run_rclone_copy` via a new, additive trailing parameter,
   `files_from: Option<&Path>`, which appends `--files-from-raw <path>` to the `rclone copy`
   invocation. `--files-from-raw` (not the plain `--files-from`) is used deliberately:
   entries here are literal filenames straight from an `rclone lsjson` listing, not
   user-authored glob patterns, and a real filename can itself contain glob metacharacters
   (`*`, `?`, `[`) that plain `--files-from` would otherwise try to interpret as a pattern.
   Every other caller of `run_rclone_copy` (`rclone::worker::run_copy_job`) passes `None` and
   is unaffected.
4. If every matching source file is already done, `files_needing_pull` returns an empty
   list and Phase A's `rclone` subprocess is not spawned at all -- a trivial task resolves
   immediately with a zeroed `RcloneLogSummary` instead of running `rclone copy` against an
   empty files-from list.

`enqueue`'s existing post-download check against `existing_destination_filenames`
(ADR-0120 §3) stays exactly as-is as a second, independent backstop -- e.g. for a file that
somehow still lands locally despite the new filter, or one left over in `source/` from a
same-VM crash-and-restart (the initial directory scan, `manifest::gather_pending`, still
exists for exactly that case and is untouched).

This ADR explicitly amends/supersedes ADR-0120's "Accepted limitation" bullet: Phase A no
longer re-pulls bytes for already-done files on a rerun, resolving that previously-accepted
gap rather than merely mitigating its disk-space symptom.

## Consequences

- `transform`'s local disk footprint during a run no longer scales with the total dataset
  size; it scales with the in-flight window (`--concurrency`/`--transfers` wide), directly
  fixing the "must anticipate total size of source and destination" sizing problem a volume
  for this job had to account for.
- A rerun/incremental run over a prefix with partial prior progress no longer re-downloads
  already-pushed files at all -- stronger than merely not retaining them locally, and a
  direct resolution of ADR-0120's own accepted limitation.
- No `transcode`/`push`-phase-style metric fires for Phase A when there is nothing left to
  pull (the subprocess is never spawned) -- correct, since zero events occurred, not a
  regression of ADR-0120's observability fix.
- Failed files are unaffected by either change -- still left in place for diagnosis, same as
  before this ADR.

## Out of scope

- A disk-space preflight check (`commands::job::download::check_disk_space`) for Phase A --
  superseded in importance by the steady-state footprint fix in Decision §1; not needed once
  footprint is bounded to the in-flight window rather than the whole dataset.
- Any equivalent retention/cleanup change to `pull_transform`/`deduplicate`'s own local-file
  handling -- they deliberately keep `result/` around to support `--upload-only` and are out
  of scope here.
- A `--keep-local`/retention flag -- not needed; see Decision §1.

## Verification

- `mise run ci` clean (fmt-check + lint + test), including:
  - `destination.rs`: new tests for `list_source_relative_paths` (lists nested files matching
    an extension; returns empty for a source that doesn't exist yet).
  - `rclone_transfer.rs`: a new test confirming `files_from` actually restricts a copy to
    exactly the listed relative paths via `--files-from-raw`.
  - `worker.rs`: a new unit test for `files_needing_pull`; extended the existing
    "transcodes and pushes every pending file" integration test to assert `source/`/`result/`
    are empty once every file is pushed; extended the existing "already present at the
    destination" test to assert the file is never even downloaded (not merely
    downloaded-then-skipped); extended the existing "mostly healthy batch" circuit-breaker
    test to assert every succeeded file's local source copy is gone while the one failed
    file's local source copy remains.
- Manual: run `transform` against a small local source/destination dir pair (rclone treats a
  bare path as local, no `rclone.conf` needed); watch `du -sh <local_output>` during the run
  and confirm it stays roughly flat instead of growing with total dataset size.
- Manual: run the same command a second time against the same (now populated)
  `--destination-path`; confirm via `pigeon.jsonl`/rclone's own transfer count that the
  already-pushed files are not re-downloaded, and that no new files appear in `source/` for
  them.

## Amendment (2026-10-12): `--files-from-raw` becomes an implementor field (ADR-0132)

ADR-0132 preserves this ADR's pre-pull exclusion mechanism unchanged, but
`files_from` moves from a trailing `run_rclone_copy` parameter to a field on
the `rclone::Copy` implementor, and `list_source_relative_paths`' underlying
`rclone lsjson` call merges with ADR-0120's destination listing into the
shared `rclone::Lsjson` command. `--files-from-raw` (rather than plain
`--files-from`) and the reasoning for it are untouched.
