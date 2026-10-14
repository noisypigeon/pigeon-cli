# ADR-0127: `transform` gains a concurrency guardrail, a per-process ffmpeg thread cap, and real crash-resume for `scratch/`/`result/`

- **Author**: Willow Graysen ([@noisypigeon](https://github.com/noisypigeon)).
- **Date**: 2026-10-11.
- **Status**: Accepted.

## Context

A real `pigeon job run transform --input-file-type=mov ...` run against a
14-core / 24GB MacBook was invoked with `--concurrency 50` six times in a
row (per shell history), each time OOM-killed by macOS -- the Force Quit
dialog showed the hosting terminal process (Warp, which owns the whole
`pigeon`+`ffmpeg` process tree) pinned at 161GB. Reading
`src/commands/job/transform/media.rs`'s `transcode_video` confirmed it never
passes `-threads` to `ffmpeg`, so each of up to 50 concurrent transcodes also
defaulted to claiming all 14 cores for itself -- both the process count and
the per-process thread count were oversubscribed simultaneously.
`CpuConcurrencyInput` (`src/commands/job/shared_wizard.rs`) already has a
sane cores-based *interactive* default (`default_concurrency()` via
`std::thread::available_parallelism()`), but `flag_value()` applied no
upper bound, so an explicit `--concurrency 50` was honored verbatim in
`--non-interactive` mode.

Tracing what each crashed restart actually did with its local `.staging/`
tree (ADR-0124) surfaced a second, independent bug while diagnosing the
first: `source/` is correctly rescanned and reused by `gather_pending` on
restart, and `scratch/` is correctly never reused (`transcode_video` writes
`ffmpeg`'s output directly to the `.scratch` path, so anything still there
after a crash is a truncated partial by construction, not a resumable
artifact) -- but `result/` should be reusable and wasn't checked.
`placement::place_one` atomically *renames* a finished scratch file into
`result/` only after `ffmpeg` exits 0 and passes its duration-tolerance
check, so anything present there is already fully verified-good, never
partial. If the process is killed after that rename but before push
completes, the next run had no way to know: `process_one` unconditionally
re-transcoded the same source file from scratch, and `place_one` then found
the deterministic destination name already occupied and hard-errored as a
"collision ... indicates a bug" (`placement.rs:76-83`) -- turning
legitimate resumable work into a false failure on every subsequent rerun of
that file, forever, until the stale `result/` file was manually removed.

## Decision

Four changes, landed together since they were all found diagnosing the same
incident:

1. **Clamp `CpuConcurrencyInput` against core count.**
   `shared_wizard.rs`'s `flag_value()` now clamps a supplied `--concurrency`
   to `default_concurrency() * 2` (double the physical core count -- enough
   headroom for deliberate, modest oversubscription of a partly-I/O-bound
   pipeline, but not the 50-on-14-cores case that caused this incident),
   logging a `tracing::warn!` with the requested and clamped values rather
   than erroring -- consistent with this codebase's existing precedent of
   never breaking an existing non-interactive invocation outright
   (`UploadConcurrencyInput`'s doc comment, ADR-0091 §3). `CpuConcurrencyInput`
   is the one shared `WizardInput` already reused by `transform`,
   `pull-transform`, `decrypt-files`, and `dedupe` (ADR-0090's
   generalize-and-reuse precedent), so this guardrail protects all four
   CPU-bound jobs, not just `transform`.

2. **Cap ffmpeg's own thread count per concurrent slot.** The resolved
   `concurrency` value is now threaded from `run_transform_job` through
   `process_one` into `transcode_video`, which adds `-threads <n>` where
   `n = (available_parallelism / concurrency).max(1)`, computed once per run
   rather than per call. Scoped to `transcode_video` only -- the video path
   is what actually drove this incident; `transcode_to_jpg` (png/heic) is a
   comparatively cheap single-frame operation and is left untouched.

3. **Clear stale scratch files at job start.** `run_transform_job` now
   best-effort `remove_dir_all`s `scratch_dir` before recreating it, since a
   leftover `.scratch` file from a crashed run is never a resumable artifact
   (see Context) -- this just reclaims the disk space proactively instead of
   leaving it until that same source file happens to be reprocessed.

4. **Reuse an already-placed `result/` file instead of hard-erroring on
   it.** `process_one` now checks, before transcoding, whether
   `placement::compute_destination_name`'s path already exists under
   `result_dir`; if so, it skips straight to push with that file (guaranteed
   complete by `place_one`'s atomic-rename-after-verification contract) and
   never re-invokes `ffmpeg`. `place_one`'s own collision guard is
   unchanged and still fires for its real intended case -- a *different*
   source file hashing to the same destination name -- since this check only
   ever looks up the exact name this file's own `(source_path,
   relative_path)` deterministically produces, never anyone else's.

## Consequences

- A `--concurrency` value far above the host's core count no longer causes
  unbounded process-count × thread-count oversubscription; it's silently
  (but loudly logged) brought back into a safe range for `transform`,
  `pull-transform`, `decrypt-files`, and `dedupe` alike.
- A `transform` run interrupted (OOM-killed, Ctrl-C, machine sleep) after a
  file's transcode finished but before its push completed now resumes
  cheaply on rerun -- a direct push, no wasted re-encode, no false failure
  -- instead of permanently hard-erroring that file until a human manually
  clears `result/`.
- `.staging/scratch/` no longer accumulates orphaned partial files across
  repeated crashed runs.

## Out of scope

- `transcode_to_jpg` (png/heic) does not get a `-threads` cap; it's a
  single-frame operation with a much smaller memory/CPU footprint than
  video transcoding, and wasn't implicated in this incident.
- No change to `pull-transform`/`decrypt-files`/`dedupe`'s own crash-resume
  behavior -- `deduplicate`/`pull_transform` already retain their `result/`
  tree deliberately for `--upload-only` resume (ADR-0089/0090), a different
  mechanism than `transform`'s remote-destination-listing-only design
  (`worker.rs`'s module doc comment); this ADR only closes the specific gap
  in `transform`'s own resume logic.

## Verification

- `mise run ci` clean (fmt-check + lint + test), including new/updated unit
  tests for the concurrency clamp, the `-threads` argument, and the
  skip-transcode-and-push-existing-result-file path.
- Manually re-ran the reproducing command against a small fixture source set
  with `--concurrency 50` on a lower-core-count machine and confirmed: the
  clamp warning logs, `ffmpeg`'s argument list carries a bounded `-threads`
  value, `.staging/scratch/` starts empty even with files pre-seeded there,
  and a pre-seeded `result/` file is pushed directly with no new `ffmpeg`
  invocation and no collision error.

## Amendment (2026-10-14): crash resume deliberately excludes case variants (ADR-0152)

This ADR's resume rests on `manifest::gather_pending` scanning `source/` and
reconstructing each file's key from its path. ADR-0152 pulls a case-colliding
key to a *disambiguated* path, which that reconstruction would get wrong -- so
those files are deliberately kept out of `source/` and are therefore invisible
to this resume. After a crash they are simply re-pulled, one small `copyto`
each.

That is the point rather than a shortfall: the alternative is an on-disk
key-to-path side map, and keeping variants outside `source/` avoids needing
one. This ADR's clear-stale-scratch step gains the variant tree on the same
reasoning -- a variant left behind is not resumable state, because nothing
records which key it came from.
