# CLI-0129: `transform` splits download/upload concurrency and adds dynamic backpressure

- **Author**: Claude ([@anthropics](https://github.com/anthropics)).
- **Date**: 2026-10-11.
- **Status**: Proposed.

## Context

A real `pigeon job run transform` run hit disk pressure: `.staging/source/` grew
faster than files could be transcoded and pushed, because the pull phase
(Phase A, one bulk `rclone copy` subprocess) downloads as fast as its own
internal parallelism allows with no awareness of how fast transcode+push is
draining the backlog.

Investigating the code surfaced two compounding problems, not one:

1. **`--transfers` is accidentally doing double duty.** `TransformPlan`
   (`src/commands/job/transform/worker.rs:137-149`) carries one `transfers`/
   `checkers` pair. `transfers`/`checkers` feed Phase A's bulk `rclone copy`
   (`rclone_transfer::run_rclone_copy`, `worker.rs:519-545`) as rclone's own
   `--transfers`/`--checkers` flags. But `transfers` is *also* reused
   (`worker.rs:557`) to size `push_semaphore`, the in-process
   `tokio::Semaphore` that caps how many concurrent per-file `rclone copyto`
   push subprocesses (`push.rs`) can run. So today there is no way to turn
   the download rate down without also throttling the upload side, or vice
   versa — the user's original ask (`--download-transfers`/
   `--download-checkers` + `--upload-transfers`) names exactly this gap.
   `checkers` has no upload-side equivalent today; `copyto` never gets passed
   `--checkers` at all. `Job::run`'s `upload_concurrency: usize` parameter
   (CLI-0091's generalized upload-concurrency hook, already plumbed through
   every other upload-capable job) is accepted by `TransformJob::run`
   (`mod.rs:74-81`) but explicitly ignored, with a comment noting `transform`
   has no flag wired to it yet.

2. **Even with independent transfers/checkers knobs, nothing *dynamically*
   stops the pull phase from outrunning the pipeline.** By the time a file
   shows up on the live-tail channel (`on_copied`, `worker.rs`'s module doc
   comment), rclone has *already* written its bytes to `source/` — a lower
   static `--download-transfers` only slows the steady-state rate, it can't
   react to a backlog that's already built up faster than expected (e.g. a
   batch of large video files hitting a slow transcode phase). Dynamic,
   self-correcting pausing is needed instead, not just a lower static number
   to guess ahead of time.

Both are addressed together in this ADR.

## Decision

### 1. Split download/upload concurrency knobs

- Rename `--transfers`/`--checkers` → `--download-transfers`/
  `--download-checkers` (same defaults, 8/16 — `TRANSFERS_DEFAULT`/
  `CHECKERS_DEFAULT` in `transform/wizard.rs`, renamed to
  `DOWNLOAD_TRANSFERS_DEFAULT`/`DOWNLOAD_CHECKERS_DEFAULT`). Breaking change,
  no migration shim — consistent with this codebase's standing precedent
  for a single-contributor CLI (e.g. CLI-0017). These continue to feed only
  Phase A's bulk `rclone copy`.
- Add `--upload-transfers` (new `UploadTransfersInput` in
  `transform/wizard.rs`, default 8 — matching today's effective behavior
  since `plan.transfers` already sized `push_semaphore`). Thread it through
  the *already-existing* `Job::run(self, plan, concurrency,
  upload_concurrency)` trait parameter instead of adding a new `TransformPlan`
  field: `wizard.rs`'s `dispatch_async` calls `job.run(plan, concurrency,
  upload_transfers)` instead of today's hardcoded `job.run(plan, concurrency,
  1)`, and `TransformJob::run` finally honors `upload_concurrency` to size
  `push_semaphore` (`worker.rs:557`) instead of reusing `plan.transfers`.
  This removes the "ignores upload_concurrency, no flag wired" special case
  from `TransformJob::run`'s doc comment and brings `transform` in line with
  every other upload-capable job's use of this trait parameter.
- **No `--upload-checkers`.** `transform`'s push phase is always a single
  file per `rclone copyto` invocation (`push.rs`) — rclone's `--checkers`
  controls parallel listing/comparison across *many* files in one
  invocation, which never applies to a one-file `copyto` call. Exposing it
  would be a flag that's silently inert; deliberately not added (see Out of
  scope).
- Fix the `cli.rs` doc comments that currently (and now incorrectly) claim
  `--checkers`/`--tpslimit` are "used by both the pull and push rclone
  invocations" — `--tpslimit` stays a single, pull-only knob (its doc
  comment gets corrected to say so explicitly); `--download-checkers`
  becomes clearly pull-only by name.

### 2. Dynamic download backpressure

Add `--max-pending-downloads` (`Option<usize>`, new `MaxPendingDownloadsInput`
in `transform/wizard.rs`; non-interactive fallback defaults to
`concurrency * 4`, scaling automatically with whatever `--concurrency` the
run already chose rather than requiring a second number to hand-tune).

- **Backlog metric:** `queue.len() + in_flight.len()` in
  `run_transform_job`'s dispatch loop (`worker.rs`) — the count of files
  already downloaded to local disk but not yet confirmed pushed. Count-based,
  consistent with every other knob in this job (`--concurrency`,
  `--download-transfers`), not byte-based (a true disk-space-aware gate was
  already considered and explicitly deprioritized by CLI-0124's "Out of
  scope").
- **Pause mechanism:** `rclone_transfer::run_rclone_copy` gains one new,
  additive, optional trailing parameter — `pid_tx:
  Option<tokio::sync::oneshot::Sender<u32>>` — sent the child's PID
  immediately after `Command::spawn()` succeeds. Every other caller
  (`rclone::worker::run_copy_job`) passes `None` and is unaffected, same
  shape as CLI-0124's own `files_from` addition. `worker.rs` holds the
  receiving end and, once it has the pull subprocess's PID, can pause/resume
  it independently of `run_rclone_copy`'s own poll loop.
- Pause/resume is driven from inside `run_transform_job`'s existing
  `tokio::select!` loop: once backlog reaches the high-water mark
  (`max_pending_downloads`) and the pull isn't already paused, send
  `SIGSTOP` to the pull PID (new `libc` dependency — `libc::kill(pid as
  i32, libc::SIGSTOP)`); once backlog drops to the low-water mark (half of
  `max_pending_downloads`, to avoid thrashing right at the boundary) and the
  pull is paused, send `SIGCONT`. Unix-only (`SIGSTOP`/`SIGCONT` don't exist
  on Windows) — acceptable since this repo's release matrix
  (`.github/workflows/release.yml`) only ever targets macOS and Linux.
  `kill` failures (e.g. `ESRCH` from a PID that already exited) are
  best-effort `tracing::warn!`, never fatal — same posture as CLI-0068.
- **Deadlock guard:** the dispatch loop's exit condition already requires
  `pull_finished` before breaking, even once the CLI-0121 circuit breaker
  trips. If the pull is paused when the circuit breaker trips, dispatch of
  new files from `queue` halts (per the breaker's existing behavior) and the
  backlog can never drain below the low-water mark on its own — so the
  paused pull would hang forever and the job could never finish. Fix:
  tripping the circuit breaker unconditionally issues a resume (`SIGCONT`)
  if the pull is currently paused, before continuing to drain in-flight
  work, regardless of backlog.
- Both high/low water marks and current paused state are plain local
  variables in `run_transform_job`, no new shared/locking state needed —
  the same single-threaded dispatch loop already owns `queue`/`in_flight`.

## Consequences

- `--download-transfers`/`--download-checkers` can be set independently of
  `--upload-transfers`, so a disk-constrained run can throttle the download
  side without starving the upload side's ability to drain the backlog —
  directly actionable manual tuning for the reported incident.
- By default (no flags needed), a run no longer silently outgrows local
  disk: once downloaded-but-unprocessed files cross `concurrency * 4`, the
  pull subprocess is automatically paused until transcode+push catches up,
  then resumes — self-correcting instead of requiring the operator to
  predict the right static transfer count ahead of time.
- `transform` finally uses the `Job::run` trait's `upload_concurrency`
  parameter like every other upload-capable job, removing a standing
  special case.
- A paused pull subprocess holds its TCP connections idle for the pause
  duration; rclone's own `--retries`/`--low-level-retries` are expected to
  absorb any resulting connection hiccup on resume, same as any other
  transient network interruption it already tolerates. Not expected to be a
  practical problem, but noted as an accepted trade-off of signal-based
  pausing versus, say, never starting the download in the first place.
- `transform`'s CLI surface picks up one new always-on behavior
  (`--max-pending-downloads`'s default) that didn't exist before; a
  non-interactive run that's never hit this ceiling before will not change
  behavior at all (the default scales with `--concurrency`, and only
  activates once a real backlog forms).

## Out of scope

- `--upload-checkers` — no file-count for rclone's `--checkers` to
  parallelize against in a single-file `copyto` invocation; see Decision §1.
- Restructuring the push phase from per-file `rclone copyto` into a bulk,
  batched `rclone copy` (which *would* give `--upload-transfers`/
  `--upload-checkers` native rclone-level parallelism semantics) — a much
  larger change to CLI-0116's per-file-pipelining design, not needed to
  solve the reported disk-pressure problem.
- Byte/disk-space-aware backpressure (vs. this ADR's file-count backlog) —
  already deprioritized by CLI-0124's own "Out of scope" once that ADR's
  per-file cleanup bounded steady-state usage to the in-flight window; this
  ADR's backpressure further bounds how large that window can get, which is
  the more precise fix for the same underlying concern.
- Any change to `rclone::worker::run_copy_job`'s own behavior — it passes
  `None` for the new `pid_tx` parameter and is otherwise untouched.

## Verification

- `mise run ci` clean (fmt-check + lint + test), including:
  - `transform/wizard.rs`: new tests for `UploadTransfersInput`/
    `MaxPendingDownloadsInput` flag-value/non-interactive-fallback behavior
    (mirroring existing `TransfersInput`/`CheckersInput` tests), and that
    `MaxPendingDownloadsInput`'s fallback scales with a given concurrency.
  - `worker.rs`: unit tests for the pause/resume threshold decision (pure,
    given a backlog count and the high/low water marks) and for the
    circuit-breaker-triggers-unconditional-resume guard.
  - `rclone_transfer.rs`: a test confirming `pid_tx`, when given, receives
    a real PID after a local-filesystem copy.
  - `push.rs`/`mod.rs`: updated to confirm `push_semaphore` is sized from
    the resolved upload-transfers value, not `plan.transfers`.
- Manual: run `transform` against a local source/destination directory pair
  with a deliberately slow transcode (e.g. a large `--video-quality high`
  batch) and a low `--max-pending-downloads`; confirm via `pigeon.jsonl`
  and `ps`/`top` that the `rclone` pull process visibly stops (state `T` on
  Linux, `kill -0` still succeeds) once the threshold is hit, and resumes
  once backlog drains.
- Manual: confirm `--download-transfers 1 --upload-transfers 8` (or the
  reverse) actually produces independent pull/push concurrency by watching
  `pigeon_job_phase_total`/`pigeon_upload_*` metrics during a run.

## Amendment (2026-10-12): the PID channel becomes an implementor field (CLI-0132)

CLI-0132 preserves this ADR's SIGSTOP/SIGCONT backpressure mechanism exactly;
`pid_tx` moves from a trailing `run_rclone_copy` parameter into the
`StreamHooks` field on the `rclone::Copy` implementor, `Default`-empty for
every command that does not want it. The split download/upload concurrency
flags and the pause/resume water marks are unaffected.

## Amendment (2026-10-14): backpressure stops signalling a finished pull (CLI-0151)

The pause/resume block this ADR added had no `pull_finished` guard, so it
signalled the pull process regardless of whether it still existed. On a real
run:

```text
04:08:14.595  rclone: copy complete (transferred 266, errors 0)
04:08:14.962  WARN transform: failed to pause pull process  "kill SIGSTOP failed"
04:13:43.677  WARN transform: failed to resume pull process
```

367 ms after the pull exited, the backlog was still above the high-water
mark, so the job SIGSTOPped the dead pid -- then believed the pull was paused
for the next 5 m 29 s and tried to resume it. Two misleading WARNs per run,
reliably triggered whenever the pull outpaces local work, which small inputs
guarantee (here: pull 11 s, local work 5 m 35 s).

CLI-0151 guards the block on `!pull_finished` and resets `pull_is_paused`
when the pull completes -- an exited process is not paused. The water marks,
the `--max-pending-downloads` default, and the deadlock guard on a tripped
circuit breaker are unchanged.
