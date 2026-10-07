# ADR-0104: fix report-bucket artifact upload ordering so a run's own outcome is actually in it

- **Author**: Willow Graysen ([@noisypigeon](https://github.com/noisypigeon)).
- **Date**: 2026-10-07.
- **Status**: Proposed.

## Context

33 `job run deduplicate`/`job run import` run bundles (2026-10-05 through
2026-10-07, uploaded to the ADR-0100 report bucket) were reviewed end to end.
Across every single one, the archived `pigeon.jsonl` snapshot is missing its
own run's `"command finished"` line -- the one event that records the exit
code and `elapsed_ms` for that run. Confirmed by `grep -l "command finished"`
across all 33 `pigeon.jsonl` files: the handful of hits that exist belong to a
*different, earlier* leg sharing the same multi-leg VM and shared log file,
never to the run the bundle is named after.

This is structural, not a flaky timing issue, confirmed by reading the code:

- `run_instrumented` (`src/observability/mod.rs:134-151`) wraps a job's entire
  dispatch closure `f`: it logs `"command started"`, calls `f()`, and only
  *after `f()` returns* logs `"command finished"` (with `exit_code`,
  `elapsed_ms`) and lets the `command` span close.
- `report_upload::upload_run_artifacts` (`src/commands/job/report_upload.rs:106-134`)
  -- which uploads the current `pigeon.jsonl` log file, the report, and the
  transcript to the report bucket (ADR-0100) -- is called and `.await`ed
  *inside* `dispatch_async`, i.e. inside `f()`, before `f()` returns to
  `run_instrumented`. Confirmed at both call sites:
  `src/commands/job/deduplicate/wizard.rs:258-296` and
  `src/commands/job/import/wizard.rs:216-242`.

So the log snapshot that gets uploaded is always taken *before* the one log
line that says the run finished and how. This holds for every job type that
follows this `dispatch_async` + `upload_run_artifacts` pattern -- it isn't
specific to `deduplicate` or `import`, just the two job types this review
happened to sample.

`deduplicate` compounds this with a second, independent gap: its
phase-boundary logging (`src/commands/job/deduplicate/worker.rs`) has exactly
four `tracing::info!` calls, by ADR-0099's deliberate scope -- download/
expand/hash phase starting (line 328) and complete (line 506), placement
phase complete (line 542), and upload phase starting (line 569). There is no
"upload phase complete" line at all. Even if the ordering bug above were
fixed, the archived log for a `deduplicate` run would still be silent on how
its longest, most failure-prone phase (per ADR-0091's upload-phase timeout/
retry work) actually went -- only the separately-written `deduplicate-report.txt`
and `transcript.txt` carry that, and `transcript.txt` has its own gap (ADR-0105).

Net effect: the one artifact ADR-0100 introduced specifically to make a run's
outcome self-contained and auditable from the report bucket alone cannot
actually answer "did this run finish, and how" -- an operator has to fall
back to the live/ambient log on the host (if it's even still running or the
VM hasn't self-deleted -- see the companion deployment ADR in
`noisypigeon/noisypigeon` about job VMs destroying themselves on exit) or to
`transcript.txt`/the bespoke report, which themselves have gaps on the
failure path (ADR-0105).

## Decision

1. **Capture and log the run's own outcome before uploading artifacts, not
   after.** Each job wizard's `dispatch_async` already computes `exit_code`
   (and, on `Err`, the error) before calling `upload_run_artifacts`. Add a
   `tracing::info!`/`tracing::error!` line there -- logging the same
   `exit_code`/outcome `run_instrumented` would otherwise log on return --
   immediately before the artifact upload, so the uploaded `pigeon.jsonl`
   snapshot always contains at least one line stating how the run ended, from
   inside the span that's still open. `run_instrumented`'s own post-return
   `"command finished"` stays as-is (it's still correct and useful for the
   *live*/ambient log); this is additive, not a replacement.
2. **Give `deduplicate` an "upload phase complete" log line**, matching the
   shape of its three existing phase-boundary lines (counts: uploaded,
   unchanged, upload_failed), closing the gap ADR-0099 left out of its
   deliberately-narrow four-line scope.
3. Audit every other job type sharing the `dispatch_async` +
   `upload_run_artifacts` pattern (`pull-transform`, `email-sync`,
   `email-pull`, `decrypt-files`) for the same ordering issue and apply the
   same fix uniformly, rather than fixing only the two job types this
   review's sample happened to cover.

## Consequences

- An operator (or a future `analyze-job-run` skill invocation) can determine
  a run's final outcome from the report-bucket artifacts alone, without
  needing access to the live host or the shared ambient log.
- `deduplicate`'s archived log gains visibility into its upload phase's
  duration and outcome, matching the other three phases.
- No change to exit codes, checkpointing, or any user-facing behavior other
  than log/artifact content -- purely an observability fix.

## Out of scope

- Whether `upload_run_artifacts`'s upload itself should retry harder or
  surface failures differently -- unchanged, still best-effort per ADR-0100.
- Restructuring `run_instrumented` itself (e.g. moving artifact upload to run
  *after* it) is a larger refactor touching every job wizard's control flow;
  the fix above (log the outcome before uploading, from inside
  `dispatch_async`) achieves the same observability goal without it.

## Verification

- Unit/integration test: after a simulated job run (success and failure
  paths), assert the uploaded log file (or the log file at the point
  `upload_run_artifacts` is called) contains a line recording the run's own
  outcome.
- Manual: run `job run deduplicate` and `job run import` against a small
  local bucket, inspect the uploaded `pigeon.jsonl` in the report bucket, and
  confirm it shows this run's own completion and (for deduplicate) upload
  phase completion.
- `mise run ci` clean.
