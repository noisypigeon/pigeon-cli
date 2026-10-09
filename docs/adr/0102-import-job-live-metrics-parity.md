# ADR-0102: `import` job live metrics parity

- **Author**: Willow Graysen ([@noisypigeon](https://github.com/noisypigeon)).
- **Date**: 2026-10-06.
- **Status**: Accepted.

## Context

ADR-0101 added `pigeon job run import` and wired it into `observability::metrics::record_phase_count("import", "transfer", ..., None)`, but only once, after the `rclone copy` subprocess has fully exited (`rclone_log::parse_and_report` reads the whole `--use-json-log` file in a single pass). Auditing import against the other 6 job types surfaced three concrete gaps, none of them deliberate decisions in ADR-0101's text:

1. **`pigeon_job_macro_phase` never appears for import at all.** Every other job calls `observability::metrics::set_macro_phase(job, ...)` once at job entry and again when its upload phase starts (`upload.rs`'s `run_upload_phase`, the single choke point all 5 S3-upload-capable jobs funnel through). Import never touches `upload.rs` -- its "transfer" is `rclone copy` itself -- and never calls `set_macro_phase` at all, so `pigeon_job_macro_phase{pigeon_job="import"}` simply doesn't exist on `/metrics`; a dashboard panel keyed on this gauge silently omits import, indistinguishable from "import never ran."
2. **`pigeon_upload_bytes_total`/`pigeon_upload_outcomes_total` never appear for import.** These are emitted by `upload.rs::upload_one` for every other job's per-file uploads. Import moves real bytes (already parsed into `RcloneLogSummary.bytes`) and has real per-run success/failure counts (`transferred`/`errors`), but reports neither under these metric names -- a "total bytes moved across all jobs" or "success/fail rate by job" dashboard built on `pigeon_upload_*` undercounts or omits import's work entirely.
3. **Every metric fires once, after the subprocess has already exited**, not live as the transfer progresses. ADR-0093 introduced `pigeon_job_phase_total` specifically to replace a once-at-the-end tally with a live signal ("a dashboard built on that never showed anything mid-run"); import still has that exact problem. For a multi-hour `rclone copy`, `/metrics` shows nothing moving for the run's entire duration, then jumps straight to the final total.

Fixing (3) requires reading the rclone JSON log file while the subprocess is still running, which ADR-0101's "Out of scope" section explicitly excluded: "Live-tailing the log file for a real-time progress bar." That bullet was about a progress bar UI; this ADR reverses it for metrics purposes only -- import still gets no `indicatif` progress bar, matching every prior run's terminal output.

## Decision

### 1. Incrementally tail the log file instead of parsing it once

`src/commands/job/import/rclone_log.rs`'s one-shot `parse_and_report` is replaced by `RcloneLogTailer`, a small stateful reader that can be `poll()`ed repeatedly as the file grows: it tracks a byte offset (so each poll only reads what's new, not the whole file again), a buffered partial line (the file's tail may be mid-write, not yet newline-terminated, when a poll lands), and the last-seen cumulative `transferred`/`errors`/`bytes` totals (rclone's `stats` lines are cumulative-since-start, not deltas). `poll()` returns a `TailDelta` -- what changed since the previous call -- so the caller emits metrics from deltas and never double-counts across polls. A log file that doesn't exist yet (rclone opens it slightly after the subprocess is spawned) is treated as "no new data," not an error. Per-object `"level":"error"` lines are now re-emitted as `tracing::warn!` as soon as they're read, instead of batched at the very end.

### 2. `worker.rs` spawns the subprocess and polls alongside it

`run_import_job` replaces `Command::...output().await` with `Command::...spawn()`, piping `stderr` and draining it concurrently into a `tokio::spawn`'d task (so a chatty subprocess can't deadlock on a full pipe nobody's reading, which `.output()` previously handled automatically). A `tokio::select!` loop races a `tokio::time::interval` tick (every 10s -- independent of rclone's own fixed `--stats 30s`, just frequent enough that an error line surfaces close to real time) against the child's exit. Each tick calls `tailer.poll()` and emits any non-zero delta as metrics; one final `poll()` after exit flushes whatever was written between the last tick and process exit. The final `ImportSummary`/`"import: rclone copy complete"` log line is built from `tailer.summary()`'s last-known cumulative totals, same shape as before.

### 3. `set_macro_phase("import", true)` at spawn, not at entry

Every other job calls `set_macro_phase(job, false)` at entry and flips it to `true` only once local work (download/hash/placement) finishes and uploading starts -- a real two-phase split. Import has no such split: there's no separate "local work" stage, rclone's copy *is* the entire job. `set_macro_phase("import", true)` is called once, immediately before spawning the subprocess, rather than following the `false`-then-`true` pattern that doesn't apply here.

### 4. New per-poll metrics, reusing the existing `"n/a"` bucket sentinel

Each non-zero `TailDelta` now also emits, alongside the existing `record_phase_count("import", "transfer", "transferred"/"failed", delta, None)`:

- `pigeon_upload_bytes_total{pigeon_job="import", destination_bucket="n/a"}` -- incremented by `delta.bytes`.
- `pigeon_upload_outcomes_total{pigeon_job="import", outcome="uploaded", destination_bucket="n/a"}` -- incremented by `delta.transferred`.
- `pigeon_upload_outcomes_total{pigeon_job="import", outcome="failed", destination_bucket="n/a"}` -- incremented by `delta.errors`.

`destination_bucket` reuses `observability::metrics::NO_BUCKET` (now `pub(crate)`, previously private to that module) rather than a raw rclone connection string: there's no `BucketConfig` on either side of an `import` run (ADR-0101 §1, unchanged), and a raw `--source`/`--destination` string varies per invocation in a way that would make this metric's label set effectively unbounded -- the same cardinality reasoning that led ADR-0097 to use a fixed sentinel for `source_bucket` rather than any other per-run identifier.

### 5. `pigeon_upload_attempts_total`/`pigeon_upload_duration_seconds` stay unimplemented for import

These two are genuinely per-file measurements on every other job (an attempt counter and a latency histogram observed once per uploaded file in `upload.rs::upload_one`). rclone's `--use-json-log` at `--log-level INFO` doesn't expose a per-file "attempt" event or per-file duration without also enabling `-v`-level object-transfer logging, which this job doesn't turn on (ADR-0101 fixed the flag set deliberately). Faking these two metrics from the aggregate `transferred`/`errors` counts would misrepresent their semantics everywhere else they're read (e.g. an "attempts per file" ratio, or a p99 latency histogram) for the sake of filling in two names. Left as a genuinely deferred gap, not a permanent boundary -- see Out of scope.

## Consequences

- `pigeon_job_macro_phase`, `pigeon_upload_bytes_total`, and `pigeon_upload_outcomes_total` now appear for `pigeon_job="import"` just like the other 6 job types; a dashboard built on any of these three no longer silently omits import's runs.
- Metrics for a long-running import now update roughly every 10 seconds while the transfer is in progress, not only once at the very end -- closing the same "nothing visible mid-run" gap ADR-0093 fixed for the other 6 jobs.
- `run_import_job` is structurally more complex (`spawn` + concurrent stderr drain + poll loop, replacing a single `.output().await` call), trading simplicity for live visibility.
- `pigeon_upload_attempts_total`/`pigeon_upload_duration_seconds` remain absent for `pigeon_job="import"` -- any dashboard panel built on either of those two specifically (as opposed to `pigeon_upload_bytes_total`/`outcomes_total`, now fixed) still won't show import.
- The rclone JSON log schema mismatch risk ADR-0101 already flagged (a schema change silently zeroing parsed counts) now also silently zeroes the live per-poll deltas -- same risk, just checked more often.

## Out of scope

- Implementing `pigeon_upload_attempts_total`/`pigeon_upload_duration_seconds` for import -- would require enabling rclone's `-v`-level per-object transfer logging and parsing per-file completion events instead of only periodic `stats` lines and error lines, a larger change to the fixed flag set ADR-0101 deliberately chose not to expose as configurable. ([#28](https://github.com/noisypigeon/pigeon-cli/issues/28))
- A real-time progress bar for import -- this ADR tails the log for metrics only; `import` still prints no `indicatif` bar, same as before.
- Classifying rclone's distinct non-zero exit codes into different pigeon-level outcomes (unchanged from ADR-0101).

## Verification

- `mise run ci` clean (fmt-check + lint + test).
- `rclone_log.rs` unit tests: a line split across two polls is counted exactly once (not double-counted, not dropped); a poll against a not-yet-created log file returns a zeroed delta instead of erroring; a second poll with no new log data returns a zeroed delta; existing error-line/garbage-tolerance/last-stats-wins coverage still passes against the new incremental API.
- `worker.rs`'s existing `copies_a_file_between_two_local_directories` end-to-end test (skipped if `rclone` isn't on `PATH`) still passes under the new spawn+poll model.
- Manual run against a real `rclone.conf`-backed remote with a multi-minute transfer: `curl localhost:9091/metrics` shows `pigeon_job_macro_phase{pigeon_job="import"}` at `1`, and `pigeon_upload_bytes_total`/`pigeon_job_phase_total` counts increasing across multiple distinct scrapes while the transfer is still running, not just once at the end; an induced per-object error surfaces as a `tracing::warn!` promptly rather than only after exit.

## Amendment (2026-10-09): restructured into `rclone copy`/`rclone delete` (ADR-0110)

ADR-0110 renames this job `"import"` -> `"rclone-copy"` and adds a sibling
`"rclone-delete"` action. The live-tailing mechanism this ADR introduced
(`RcloneLogTailer`, the `tokio::select!` poll loop, per-poll delta metrics)
is preserved unchanged in spirit, generalized to parameterize per-action
metric names/labels and add a `deletes` counter for the new action.
