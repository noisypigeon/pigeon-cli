# ADR-0116: `transform` gets a per-file pull→transcode→push pipeline, transcode retries, and no whole-run abort on one bad file

- **Author**: Willow Graysen ([@noisypigeon](https://github.com/noisypigeon)).
- **Date**: 2026-10-10.
- **Status**: Proposed.

## Context

A real `pigeon job run transform --input-file-type=png ...` run pulled 11,731
PNGs. `transform-report.txt` shows 1,605 files transcoded successfully and
exactly **one** genuinely corrupt source PNG failing ffmpeg's decode:

```
2022-10-25_Return_your_Mack_Weldon_items_to_our_store_1.png	failed		ffmpeg failed to transcode /mnt/data/a/source/2022-10-25_Return_your_Mack_Weldon_items_to_our_store_1.png: [png @ 0x55860706c380] chunk too big
[png @ 0x55860707fe00] chunk too big
Error while decoding stream #0:0: Invalid data found when processing input
```

This is a real data problem -- a truncated/corrupted source file -- not a
pigeon bug, and not a recurrence of ADR-0115's already-fixed scratch-path
muxer issue (that fix, `-f mjpeg`, is already present and working correctly
here). But `transcript.txt`'s final line reads:

```
Pulled 11731 file(s); 1605 transcoded, 0 copied through, 1 failed; pushed 0 file(s).
```

**Zero files pushed.** Tracing `src/commands/job/transform/worker.rs`
confirms why, and it is worse than it first looks:

- ADR-0112 Decision §7 deliberately chose a fail-fast design: `run_phase_b`
  stops *dispatching new files* the instant any file's transcode fails,
  though already-in-flight tasks (bounded by `--concurrency`) are allowed to
  drain. That is why 13 more files transcoded successfully *after* the
  failed row in the report -- they were already in flight when dispatch
  stopped.
- Because dispatch stops at the first failure, **the other ~10,125 of the
  11,731 pulled files were never even attempted** this run -- not failed,
  just never looked at.
- Worse: `run_transform_job` returns early with `pushed: 0` whenever any
  failure occurred -- Phase C (the bulk `rclone copy` push) **never runs at
  all**, so all 1,605 successfully-transcoded files were silently discarded,
  not just the one genuinely corrupt file.
- `Job::run` returns `Ok`, not `Err`, by design (so every already-succeeded
  file's outcome survives for the report) -- so nothing in the terminal
  output looks like a crash. Only a nonzero process exit code (easy to miss
  in automation) signals anything went wrong, and even that gives no hint
  that 1,605 good files were thrown away rather than pushed.

This ADR fixes three things together, since they are one coherent failure
mode once traced to its root:

1. **Retries** for the one step that currently has none at the app level:
   the `ffmpeg` transcode call. (rclone's pull/push already carry
   `--retries 5 --low-level-retries 20` internally, per
   `rclone_transfer::run_rclone_copy`.)
2. **Recovery**: a single file's failure (after retries exhaust) is recorded
   and reported, but never stops any other file's processing or pushing.
   The job's exit code still reflects "did anything fail," but no longer at
   the cost of everyone else's work.
3. **Per-file pipeline**: download, transcode, and upload run independently
   per file instead of as three monolithic phases each waiting for the
   whole batch to finish before the next can start.

A design trade-off was considered explicitly before committing to an
approach. A "fully independent per-file" design -- a separate `rclone
copyto` subprocess invocation per file for *both* the download and the
upload leg -- was rejected: at this run's scale that is ~23,000 extra
subprocess/connection round-trips versus today's 2 bulk `rclone copy`
invocations, with no benefit on the download side, where one bulk copy
already parallelizes efficiently via rclone's own `--transfers`/`--checkers`.
The chosen design is a **hybrid**: keep one bulk `rclone copy` for download
(unchanged invocation, still efficient), but consume it *live* instead of
blocking on its exit, so transcoding starts on each file as it lands rather
than after the whole batch finishes; transcode and push become genuinely
per-file and immediate, with push going out via a new single-file `rclone
copyto` call right after each file transcodes -- never waiting for the rest
of the batch.

This ADR amends ADR-0112 §2 (the three-phase model), §6 (checkpoint scope),
and §7 (fail-fast-abort) explicitly, per this repo's convention of stating a
reversal rather than silently diverging.

## Decision

### 1. Detecting "this file just finished downloading" via rclone's own JSON log (`src/commands/job/rclone_log.rs`)

`RcloneLogTailer` today only tracks cumulative `stats` lines and per-object
`error` lines. Rclone's `--use-json-log` output already emits
`{"level":"info","msg":"Copied (new)","object":"<relative-path>",...}` (and
`"Copied (replaced existing)"`) the instant a file's transfer finishes --
confirmed directly in the production log this ADR traces. This is the
authoritative, race-free "this object's bytes are now fully and durably
present" signal, and is used instead of polling the destination directory
for newly-appeared files, which would risk reading a file rclone is still
mid-write to -- nothing in this codebase currently verifies or depends on
rclone's temp-file/atomic-rename behavior as a correctness guarantee, and
this ADR does not start relying on it either.

`TailDelta` gains `copied_objects: Vec<String>`; `TailDelta::is_empty()`
stays keyed on the four numeric fields only (`copied_objects` is an
independent signal, same treatment `collapsed_repeats` already gets).
`process_line` gains a branch: an info-level line whose `msg` starts with
`"Copied "` and carries a non-`None` `object` pushes that relative path into
a `pending_copied` buffer, drained into `TailDelta::copied_objects` on every
`poll()` (including every early-return path, so nothing is dropped).

This is purely additive -- no existing test asserts whole-struct equality on
`TailDelta`, every existing assertion inspects individual fields -- so every
caller that doesn't opt in sees no behavior change at all.

### 2. Plumbing completions out of `run_rclone_copy` (`src/commands/job/rclone_transfer.rs`)

`run_rclone_copy` gains one new trailing parameter:
`on_copied: Option<tokio::sync::mpsc::UnboundedSender<String>>`. Inside the
existing poll loop, immediately after each `emit_delta_metrics(...)` call
(both the periodic-tick call and the final post-exit call), every entry in
`delta.copied_objects` is forwarded to the sender if present; a send error
(receiver already dropped) is ignored, not treated as a failure of the
rclone subprocess itself.

Every existing caller passes `None` and is otherwise completely unaffected:
`rclone::worker::run_copy_job` (the `rclone copy`/`rclone delete` commands,
ADR-0101/0110) gets one new trailing `None` argument and nothing else
changes about its behavior, metrics, or tests. Only `transform`'s new pull
call (§3) passes `Some(sender)`.

An `UnboundedSender`/`UnboundedReceiver` channel was chosen over a
synchronous callback because the consumer (`transform`'s dispatch loop)
needs to receive these asynchronously, interleaved via `tokio::select!`
against the pull subprocess's own exit and the transcode pool's own
completions -- a channel is the natural shape for a producer on one task
feeding a consumer on another. `Unbounded` because the volume is bounded by
the number of files in the run (one send per *file*, not per log line or
per poll), not a hot path needing backpressure.

### 3. The per-file pipeline (`src/commands/job/transform/worker.rs`, new `push.rs`)

`run_transform_job`'s three sequential phases are replaced by one
concurrency-bounded dispatch loop fed by two sources, both producing work
for the same per-file pipeline (transcode/copy-through → place → push →
checkpoint):

- **Initial directory scan**: `manifest::gather_pending` (unchanged, reused
  verbatim) runs once, *before* Phase A's rclone subprocess is spawned, to
  pick up anything already on disk from an interrupted prior run. This
  matters because rclone's own skip-unchanged-file logic means a file
  already present and matching at the destination produces *no* "Copied"
  log line on a rerun -- the live tail alone would never see it again.
- **Live tail**: the bulk `rclone copy` pull is spawned as a background
  task instead of being awaited inline; its new completion channel (§2)
  feeds newly-landed files into the same dispatch queue as they arrive,
  overlapping download with transcode for the remainder of the batch.

A `tokio::select!` loop interleaves dispatching queued files (bounded by
`--concurrency`), draining finished pipeline tasks (success appends the
checkpoint and increments a running `pushed` counter; failure records
`Outcome::Failed` and keeps going -- no abort flag anywhere), receiving new
arrivals off the completion channel, and watching the pull subprocess's own
join handle. The loop ends when the pull subprocess has exited **and**
nothing is in flight **and** the dispatch queue is empty.

**A correctness caveat found while designing this, stated explicitly rather
than left implicit**: the two dispatch sources are disjoint in the common
case, but not watertight by construction -- rclone can in principle still
emit a "Copied" line for a file the initial scan already picked up, if it
can't confirm a size/modtime match against what's already on disk. A
`dispatched: HashSet<String>` populated at the moment a file is enqueued
from *either* source guards the live-tail branch against double-enqueueing,
on top of the existing done-checkpoint check. `placement::place_one`'s
existing hard-error-on-collision behavior remains a second, independent
backstop against any double-dispatch that slips through regardless.

**New per-file push** (`transform/push.rs`, a new sibling module matching
this directory's existing `media`/`placement`/`manifest` split):
`push_one` builds `<destination_path>/<destination_filename>` and runs a
single `rclone copyto <local_path> <destination>` subprocess call, wrapped
in `core::retry::retry_with_backoff` (`PUSH_RETRIES = 3`,
`PUSH_RETRY_BACKOFF = 2s`, matching `upload.rs`'s existing
`UPLOAD_RETRIES`/`UPLOAD_RETRY_BACKOFF` values exactly). `copyto` (not bulk
`copy`) is rclone's documented single-file-to-single-file-path primitive --
no directory listing round-trip for one named object. Each call writes to
its own small log under `<local_output>/.staging/push-logs/` -- thousands of
concurrent single-file subprocesses cannot share one `--log-file` path.
Still carries rclone's own `--retries 5 --low-level-retries 20` internally;
the outer `retry_with_backoff` wraps the whole subprocess invocation, same
justification `upload.rs` already gives for wrapping an already-internally-
retrying call (a connection-level failure before rclone's own retry logic
even engages). Emits `record_phase_count("transform", "push", ...)` directly
per file, so `pigeon_job_phase_total{phase="push"}` now updates live
per-file instead of only once, at the very end, for the whole batch.

**Push concurrency**: a `tokio::sync::Semaphore` sized from the existing
`--transfers` CLI flag bounds simultaneous push subprocesses, decoupled from
`--concurrency` (which stays the CPU-bound transcode pool, unchanged) --
mirroring ADR-0090's established CPU-vs-IO-bound concurrency split. This
deliberately does **not** use the `Job::run` trait's `upload_concurrency`
parameter, which ADR-0091 §3 built for exactly this purpose: `transform`'s
CLI surface has no `--upload-concurrency` flag today, and adding one means a
new flag plus wizard prompt for a knob `--transfers` already conceptually
covers ("how many simultaneous rclone-side transfer operations"). This is a
deliberate, acknowledged repurposing of an existing flag rather than new
dedicated plumbing -- stated here explicitly rather than left as a silent
assumption.

**Transcode retries**: `process_one`'s `media::transcode_to_jpg` call is
wrapped in `retry_with_backoff` at the call site in `worker.rs`
(`TRANSCODE_RETRIES = 2`, `TRANSCODE_RETRY_BACKOFF = 1s`) -- not inside
`media.rs` itself, mirroring `upload.rs`'s separation of retry policy from
the thing being retried. `transcode_to_jpg`'s existing `-y` flag already
makes a retried attempt safely overwrite the previous attempt's scratch
output, so no cleanup is needed between retries. A genuinely corrupt file
(like the one this ADR traces) still fails after retries exhaust -- correct,
since no amount of retrying fixes corrupted source bytes -- but a transient
failure (resource contention, a disk hiccup under concurrent load) now gets
a second chance. `copy_through` (the jpeg path, a plain `fs::copy`) is
deliberately **not** wrapped: a local filesystem failure there is not the
transient shape retries meaningfully help with, and masking it would hide a
real problem (disk full, permissions) behind pointless retries.

**Checkpoint semantics change** (amends ADR-0112 §6): `append_checkpoint`
now fires only once a file's push *also* succeeds, not merely once it is
placed under `result/`. It is still called single-threaded from the
dispatch loop's success arm, never from inside a concurrent task -- that
invariant is unchanged, only the condition gating it moves later in the
file's lifecycle. A crash between place and push now correctly leaves the
file unchecked, so a rerun's initial directory scan picks it back up and
retries the whole transcode-and-push, not just the push.

**`TransformSummary`/`Outcome`/`FileOutcome`**: no field renames.
`TransformSummary.failed` changes meaning from "did Phase B abort" to "did
any file ultimately fail" (`outcomes.iter().any(|o| o.outcome ==
Outcome::Failed)`). This is already exactly what gates the wizard's exit
code (`wizard.rs`'s `if summary.failed { FAILURE_EXIT_CODE } else { 0 }`),
so no change is needed in `wizard.rs` at all -- it was already reading the
right field; only `worker.rs`'s computation of that field changes.
`TransformSummary.pushed` becomes a running per-file counter incremented in
the dispatch loop instead of one bulk `RcloneLogSummary.transferred`.
Failure-stage distinction (transcode vs. push failure) stays in
`FileOutcome.detail`'s existing free-text string rather than widening the
`Outcome` enum -- nothing downstream parses `detail` structurally, so a new
enum variant would add ceremony with no consumer.

## Consequences

- A single corrupt (or otherwise permanently failing) file now costs exactly
  one failed report row and one unit of nonzero exit code -- never the rest
  of the batch, and never the push of every file that already succeeded.
  This is the direct fix for the incident this ADR traces.
- Push progress is now visible live, per file
  (`pigeon_job_phase_total{phase="push"}` increments as each file pushes)
  instead of only ever being reported once, at the very end, for the whole
  batch -- and only if the batch had zero failures at all, which was the
  actual production failure mode.
- Operational cost: many more `rclone` subprocess spawns for the push leg --
  one per transcoded file instead of one for the whole batch. This is the
  deliberately accepted cost of the "hybrid" design chosen over "full
  per-file" (which would have paid this cost on the download leg too, for
  no offsetting benefit) and over "minimal" (which would have left the
  whole-run-abort-on-one-file problem unaddressed).
- Many small per-file push logs now live under `<local_output>/.staging/
  push-logs/` instead of one `rclone-push-<run_id>.jsonl` -- worth flagging
  for anyone extending `analyze-job-run`-style log tooling to this job.
- `rclone::worker::run_copy_job` (`rclone copy`/`rclone delete`) is
  unaffected: `run_rclone_copy`'s new parameter is additive and defaults to
  `None` at that call site.

## Out of scope

- A structured failure-stage enum beyond `FileOutcome.detail`'s free text --
  nothing downstream parses it, so no consumer justifies the added ceremony
  today.
- A dedicated `--upload-concurrency` flag for `transform` -- deferred, not
  rejected forever; `--transfers` is reused instead for now (Decision §3).
- Any change to `pull_transform`'s own retry/fallback design -- it is
  already lenient (retries, then falls back to keeping the original file)
  and untouched here.
- Any change to `rclone copy`/`rclone delete`'s own behavior -- the new
  `on_copied` parameter on `run_rclone_copy` is additive and `None` at that
  call site.

## Verification

- `mise run ci` clean (fmt-check + lint + test), including new/rewritten
  tests for: `rclone_log.rs`'s `copied_objects` parsing; `rclone_transfer
  .rs`'s channel plumbing; a mixed-batch `transform` run (one permanently
  corrupt file among several good ones) asserting every file is attempted,
  every good file is checkpointed *and* pushed, the bad file is reported
  with a non-empty detail, and the run's overall `failed` flag is set
  without `pushed` dropping to zero; a resumed-run test proving the initial
  directory scan dispatches a file the live tail alone would never see
  again; and a double-dispatch-guard test.
- Confirm `rclone::worker::run_copy_job`'s existing tests pass unchanged.
- Manual: run `pigeon job run transform --input-file-type=heic --source-path
  <local dir with 2 good files and 1 corrupt file> --destination-path <empty
  local dir> --local-output /tmp/pigeon-transform-test --yes` against plain
  local directories (no `rclone.conf` needed); confirm the 2 good files land
  at the destination, the report lists all 3 with the corrupt one marked
  `failed` and a real ffmpeg-stderr detail, the exit code is nonzero, and
  rerunning the identical command is a no-op for the 2 already-pushed files.
