# ADR-0156: transform telemetry and slot-batched case-variant pulls

- **Author**: Willow Graysen ([@noisypigeon](https://github.com/noisypigeon)).
- **Date**: 2026-10-15.
- **Status**: Accepted.

## Context

A live `job run transform --input-file-type md` over `source:md/` (332,286
files, run id `18dec1c49ed07728-pigeon-185c1`) was healthy. ADR-0129
backpressure was holding the pull SIGSTOPped with no spurious pause/resume
WARNs, so ADR-0151's fix held, and transcode and push were progressing at
about 300 files/min, roughly 18.5 hours end to end. Reading its telemetry
turned up four defects. All four are about what the job does *before* the
pipeline runs, or what it *says* while it runs:

| # | Defect | Evidence |
|---|---|---|
| A | ADR-0152's case-variant pull is a sequential, per-key `copyto` loop that blocks the **whole** pipeline | `pull phase starting` 16:49:22 → `case-variant pull complete` 17:09:07: **19m45s** of zero local work |
| B | A per-file failure logs **nothing** for most input kinds, `md` included | `pigeon_job_phase_total{phase="transcode",outcome="failed"} 9`, **0** matching lines in `pigeon.jsonl` |
| C | The run body emits **no progress telemetry** | one line at 16:49:22, then silence for an ~18.5h run |
| D | The collision notice is one INFO **per group** | 428 of the run's 430 log lines |

**A.** `run_transform_job` awaited one `CopyTo` (a fresh `rclone` process) per
colliding key, sequentially, *before* the main loop. Until it finished, nothing
was dispatched, `copied_rx` was not drained and `pull_pid_rx` was not read, so
ADR-0129 could not engage either. 870 keys took 19m45s, ~1.36 s/key, which is
almost entirely rclone startup for tiny markdown files. Meanwhile the bulk pull
had already delivered ~31k files that nothing touched. ADR-0152 justified the
serial pass as *"a colliding group is inherently tiny (two keys in the run that
prompted this)"*. That was true for the xlsx run it was written against. This
run had **415 groups of 2, 12 of 3 and 1 of 4: 428 groups, 870 keys**.

**B.** The only per-file failure logging lived in
`retry_transcode_unless_fatal`, and `convert_one` routes only the `ffmpeg` kinds
through it. Every kind ADR-0149/0154 added (documents, spreadsheets,
`textclipping`, `tsv`), every copy-through kind, and **every** `Place` failure
for any kind reached the operator only through `transform-report.txt`. That
report is written at the very end. On a long run that means hours of silence,
and an interrupt loses it entirely ([#81](https://github.com/noisypigeon/pigeon-cli/issues/81)).

**C.** Under this run's span the JSONL held three distinct messages: the 428
collision lines, `pull phase starting` and `case-variant pull complete`. The
queue, in-flight and pause state that would explain *why* the pull was stopped
existed only as `run_transform_job` locals. `core::rclone::execute` computed a
live delta every 10 s, but fed it only to Prometheus. Prometheus is
best-effort, and 279 `Address already in use` warnings that day mean other runs
had no live signal at all.

**D.** ADR-0152 kept ADR-0144's one-line-per-group notice. At 428 groups, that
put back ~128 KB of exactly the per-run log volume ADR-0151 had just cut, and it
scales with the source.

## Decision

### 1. Slot-batch the case-variant pull (A)

Replace the per-key `CopyTo` loop with **one bulk `Copy` per collision-free
slot**, all slots run concurrently (`futures::future::join_all`).

- `assign_case_variant_slots` assigns the sorted variant list to slots greedily,
  first-fit. A key joins the first slot where **every path prefix** of it (each
  directory, then the key) is either unclaimed under case folding or claimed by
  the identical spelling. Checking prefixes as well as whole keys is
  deliberate. Two keys from *different* groups can still spell a shared
  directory differently (`Dir/x.md`, `dir/y.md`), and one directory cannot hold
  both spellings on a case-insensitive volume any more than one file can. For
  the flat key sets this job sees, the result is exactly as many slots as the
  largest group has members: **4 rclone processes instead of 870**, each with
  full `--download-transfers` parallelism.
- A slot lives at `.staging/case-variants/slot{k}/`, and a key lands at
  `slot_dir.join(key)`, with the key **verbatim** as a relative path. That is
  the same shape as the bulk pull's `source_dir.join(key)`, so recovering a key
  from a local path is still never needed. This is ADR-0152's own reasoning,
  carried from per-key directories to per-slot ones. Each slot has its own
  `slot{k}-files-from.txt` and `slot{k}-pull.jsonl` beside the slot
  directories; `write_files_from` now takes the file name.
- **A failed slot is not all-or-nothing.** rclone delivers whatever it can, and
  writes to a temporary name before renaming, so presence on disk decides each
  key. A key that landed is queued. A key that did not land inherits the
  slot's error into `case_variant_failures`, which `undelivered_detail` already
  surfaces verbatim, so its two ADR-0152 arms are unchanged.
- The slots' summaries are **added to the pull totals** (`transferred`,
  `errors`, `bytes`, `duplicate_notices`). They are real pulls, and ADR-0130's
  cross-check compares rclone's errors against undelivered files, which include
  variants.
- The slots carry no `pid_tx`. They finish before the main loop, so ADR-0129
  backpressure never applies to them.

**Kept as a barrier before the main loop.** Folding the slots into the
concurrent pull, so they stream through `copied_rx`, would remove the barrier
entirely. But it means teaching `copied_tx` to carry a base directory, turning
`pull_handle` into a `JoinSet` with summed summaries, and making the backpressure
PID a `Vec`. That is substantial surgery on this codebase's most delicate loop,
for a barrier this change already shrinks from 20 minutes to seconds. It was
considered and rejected.

### 2. Log every per-file failure once, where it is recorded (B)

- The main loop's `Err((stage, err))` arm, which every stage's failure funnels
  through, now emits `tracing::warn!("transform: file failed")` with
  `relative_path`, `stage` and `error`. It runs inside `command_span`, so
  ADR-0141's span and ADR-0151's `run_id` attach for free.
  `retry_transcode_unless_fatal`'s attempt-level lines stay as detail. The new
  line is the authoritative per-file record.
- The pull reconciliation emits the same message for undelivered files, but
  **names only the first 50** (`UNDELIVERED_LOG_LIMIT`), then one aggregate WARN
  with `named`/`not_named`. Without the cap, a pull that failed wholesale would
  write one WARN per source file. The report still names every one.

### 3. Periodic progress in both layers (C)

- **`core::rclone::execute_streamed`** emits `"rclone: {subcommand} progress"`
  with cumulative `transferred`, `errors`, `bytes` and `step` every 6th poll
  (60 s at the existing 10 s `LOG_POLL_INTERVAL`), and **only when those totals
  changed** since the last line, so a SIGSTOPped or idle transfer stays quiet.
  This covers every `Streamed` consumer at once (`Copy`, `Purge`).
- **`transform`'s main loop** gets a 60 s `interval` arm emitting
  `"transform: progress"` with `queued`, `in_flight`, `dispatched`, `completed`,
  `pushed`, `failed_transcode`/`failed_place`/`failed_push`, `pull_finished`
  and `pull_is_paused`. This one is **unconditional**: an unchanged line is
  itself the evidence of a stall, which is the question this telemetry exists to
  answer. `failed_pull` is absent because pull failures are only known after
  reconciliation.

### 4. Collapse the collision notice (D)

One aggregate INFO replaces the per-group lines, carrying `groups`, `keys`,
`largest_group`, and `examples` (the first 10 groups, sorted, so the sample is
stable across runs). ADR-0152 deliberately wanted every group named. That
complete list now lives in `transform-report.txt`, after the summary lines: a new
`TransformSummary::case_collision_groups`, one group per line, keys joined with
` | `. A report is the right home for a list that grows with the source, and a
shared log is not.

## Consequences

- The reported `md` run's 19m45s pre-pipeline stall becomes the duration of four
  concurrent bulk copies. The first transcode starts right after.
- The 4 concurrent slot copies run at `--download-transfers` each, alongside the
  still-running bulk pull, so peak download parallelism briefly rises during
  that barrier. That is bounded by the largest group size and accepted.
- Every failed file is now visible in `pigeon.jsonl` as it happens, for every
  kind and stage. On the run that prompted this, that is 9 WARNs where there
  were none.
- A long run gets a progress line about once a minute. That is ~1,100 lines over
  18 hours, small next to what ADR-0151 removed and what this ADR removes in §4.
- The per-run log artifact stops scaling with the number of colliding groups.

## Out of scope

- **Throughput** (~300 files/min for `md`, i.e. one `pandoc` plus one `rclone
  copyto` per file). Real, but by design and unrelated to these defects.
- **The Prometheus port collision** (`Address already in use`). Warn-and-continue
  is correct, and §3 removes the dependence on it.
- **Detecting colliding *directories* in the bulk pull.** ([#117](https://github.com/noisypigeon/pigeon-cli/issues/117))
  `case_insensitive_collisions` folds whole keys, so `A/x.md` and `a/y.md`
  (distinct keys under differently-cased directories) are not detected, and both
  go to the bulk pull, where rclone resolves the directory clash itself. §1's
  prefix check protects the *slots* from this, but not the bulk pull. This is
  latent: no affected source has been seen, and the reported one is flat.
  Filed per ADR-0031.
- **Flagged, then resolved separately:** `transform`'s wizard takes
  `--report-bucket` as a keyring alias (`report_upload::resolve`), while
  CLAUDE.md's ADR-0132 summary said `transform` had moved to `--report-path`.
  The code is right. ADR-0136 dropped ADR-0132 §10 before it landed, so every
  job still uses `--report-bucket`. CLAUDE.md and the ADR texts that repeated
  the claim (0112, 0130, 0133) were corrected on 2026-10-15.

## Amends

ADR-0152 (its per-key directories and sequential `copyto` pass become per-slot
directories and concurrent bulk `copy`s; its per-group notice becomes one line
plus a report listing) and ADR-0151 (the per-run log volume it cut no longer
regrows with collision count). Each gets a short forward-pointing "Amendment".

## Verification

- `mise run ci` clean (fmt-check + lint + test).
- `worker.rs`, pure tests: every slot is collision-free under case folding for
  keys **and** directory prefixes; every key lands in exactly one slot; the slot
  count equals the largest group size (groups of 2/3/4 → 4 slots); differently
  cased directories never share a slot; assignment is deterministic; slot
  directories sit outside `source/`; `path_prefixes` and
  `case_collision_groups` behave as documented; and `write_report` lists every
  group and prints nothing when there are none.
- **Not end-to-end testable, as stated in ADR-0152 and still true:** on a
  case-insensitive host the colliding fixture cannot be created, and on a
  case-sensitive one the bulk pull would deliver everything, so the slot path
  would never run and the test would pass vacuously.
- §2–§4 are tracing output, so they are not unit-tested. Manual check: rerun the
  `md` job and confirm in `pigeon.jsonl` **one** collision line, a
  `"case-variant pull complete"` with `slots: 4` within seconds of
  `"pull phase starting"`, a `"transform: file failed"` WARN for each
  `phase="transcode",outcome="failed"` metric increment, and `"transform:
  progress"` / `"rclone: copy progress"` lines at ~60 s. ADR-0124's pre-pull skip
  filter makes that rerun resume from the destination listing instead of redoing
  the whole run, since nothing here changes `compute_destination_name`.
