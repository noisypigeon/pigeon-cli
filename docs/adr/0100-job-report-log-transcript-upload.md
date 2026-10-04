# ADR-0100: job wizard report/log/transcript bucket upload

- **Author**: Willow Finch ([@noisypigeon](https://github.com/noisypigeon)).
- **Date**: 2026-10-04.
- **Status**: Accepted.

## Context

Every `pigeon job run <type>` invocation leaves its evidence of what happened scattered: `deduplicate` is the only job that writes a report file at all (`deduplicate-report.txt`, ADR-0082 §5); the structured `pigeon.jsonl` observability log (ADR-0073) is one file shared by every run of every job forever; and the live `MultiProgress` terminal output (ADR-0014/0015) is never captured anywhere -- once the terminal scrolls past it, it's gone. Operating this CLI against remote buckets/cron means the person who needs the report/log often isn't at the terminal when the run happened, and today there's nowhere for them to look afterward.

This ADR adds a mandatory wizard step to every job type that uploads the run's report, the shared observability log, and a transcript of what it printed, to a configured bucket under a dated, discoverable prefix.

## Decision

### 1. A new mandatory wizard step -- `ReportBucketInput`

Added directly to `src/commands/job/shared_wizard.rs` (skipping the usual "duplicate until the third consumer" staging, since all 6 jobs need it from day one -- the same reasoning `ConfirmInput` already got). Mirrors `SourceBucketInput`'s existing mandatory-selection shape exactly: `--report-bucket <alias>` wins outright if given; an interactive `Store::prompt_select_bucket()` prompt if omitted on a TTY; a hard `Err` non-interactively. Added to all 6 `JobType` variants in `src/commands/job/cli.rs`, including every `--upload-only` resume path -- `decrypt-files`'s first-ever bucket-config dependency.

### 2. A per-run identifier, with no new crate dependency

`report_upload::generate_run_id()` (`src/commands/job/report_upload.rs`) joins a nanosecond timestamp, this process's id, and the already-cached `observability::instance()` hostname (ADR-0097). This codebase has no `uuid`/`rand` dependency today; this is collision-resistant enough for a path segment without adding one.

### 3. The upload prefix

`{YYYY-MM-DD}-{job_name}-{run_id}`, built by `report_upload::run_prefix()`. `job_name` comes from a new `JobType::job_name()` method next to `Observable::command_name()` (`src/commands/job/cli.rs`) -- `command_name()`'s value with its `"job."` prefix stripped, rather than a 6th place in this codebase repeating the same 6 job-name literals already duplicated across every job's metrics call sites.

### 4. Report artifact

`deduplicate` keeps its existing bespoke `deduplicate-report.txt` (ADR-0082) unchanged -- it already qualifies. The other 5 job types (`reduce`, `pull-transform`, `email-sync`, `email-pull`, `decrypt-files`), which previously only held an in-memory `*Summary` struct printed once via `println!`, gain a new shared helper, `report_upload::write_summary_report()`, that `Debug`-formats that same already-computed struct to `<job_name>-report.txt` under the job's local output. On a failed run (`job.run()` returning `Err`), the same helper writes the error message itself as the report instead, so a report always exists to upload either way.

### 5. Log artifact

The whole shared `pigeon.jsonl` is uploaded as-is -- not filtered or sliced to this run's lines. `observability::init()` now caches its resolved path in a `OnceLock`, exposed via `observability::log_file_path()`, so a job can find it without recomputing `--log-file`-vs-default resolution logic itself. This is an explicitly accepted trade-off: the file travels in full on every run, growing with its entire lifetime, not just this run's slice -- simpler than inventing a run-id-based log-filtering mechanism, at the cost of re-uploading history every time.

### 6. Transcript artifact -- scoped to wizard-level narration, not every worker print

The original intent was a transcript of everything a job prints, including the per-item status lines deep in each job's `worker.rs`. Implementation surfaced a real architectural mismatch with that: `MultiProgress` isn't constructed once per run and threaded everywhere -- across the 6 job types it's independently constructed well over a dozen times (once per phase: main worker loop, dedup pass, upload phase, sink, plus several more in test code), so there is no single value to wrap. Wrapping every construction site to intercept `.println()` would have meant touching upload.rs (shared by all 5 upload-capable jobs), every job's dedup.rs, sink.rs, and download.rs, including dozens of test call sites -- a large, risky mechanical refactor for a secondary artifact.

Instead, `src/observability/transcript.rs`'s `Transcript` (a `Mutex<File>`) is opened once per run in each job's `dispatch_async`, and `report_upload::say()` is a thin wrapper that prints a line exactly like `println!` always has while also recording it. Each wizard's own narration -- the pending-count line, "Cancelled.", the final result summary, and (for `email-sync`/`email-pull`) the attachments-estimate line -- now goes through `say()` instead of a bare `println!`. The deep per-item progress-bar lines inside each job's `worker.rs` are **not** captured. This is a deliberate scope reduction from the original intent, not an oversight: a transcript of wizard-level narration (what was asked for, what the plan was, what happened) is still the meaningful, human-readable "what happened in this run" record: raw stdout/PTY byte capture was already out of scope (progress-bar redraw escape codes would make that unreadable), and this goes one step further by also skipping the live per-item status noise, keeping the transcript short and genuinely readable.

### 7. Always unencrypted

The report-bucket upload never encrypts, regardless of the job's own primary-upload encryption settings (ADR-0025/0027) -- matching ADR-0082 §0's precedent that operational artifacts stay plaintext so they're readable without the encryption key.

### 8. Upload timing and failure handling

Triggered once, right after `job.run(...)` (or `worker::run_upload_only(...)`) returns, on **both** the `Ok` and `Err` arms -- before `fail()`/exit -- so a failed run still leaves its report/log/transcript behind (mirrors ADR-0097's bracket-even-on-failure philosophy). `report_upload::upload_run_artifacts()` calls `client::upload_if_changed()` directly (ADR-0089/0091's single-file streaming path) three times -- not the batch `upload.rs`/`.uploaded`-checkpoint machinery, which exists for bulk per-job file trees these always-fresh, one-off artifacts don't need. A failure uploading these artifacts is `tracing::warn!`-logged but never changes the job's own exit code -- this is instrumentation about the run, not the run's primary deliverable.

## Consequences

- **Breaking change**: every non-interactive invocation (cron/scripts) of all 6 job types now requires `--report-bucket` -- there's no skip. A deliberate, accepted trade-off: this step is mandatory by design, not an oversight.
- `decrypt-files` gains its first bucket-config/network dependency.
- Every run leaves 3 new artifacts in the configured bucket at a deterministic, discoverable path: `report.txt`, `pigeon.jsonl`, `transcript.txt` under `{date}-{job-name}-{run-id}/`.
- `pigeon.jsonl` re-uploads in full on every single run -- bandwidth cost scales with the log's total lifetime size, not this run's slice.
- 5 job types gain a report file they didn't have before (a generic `Debug` dump of their summary struct); `deduplicate`'s stays richer (per-record merge detail).
- The transcript captures wizard-level narration only, not every per-item worker status line -- a smaller artifact than originally envisioned, chosen over a large cross-cutting `MultiProgress` refactor once that refactor's real scope became clear.

## Out of scope

- Slicing/filtering `pigeon.jsonl` to just this run's lines.
- Encrypting the report-bucket upload.
- Raw byte-for-byte stdout/PTY capture, or capturing per-item `MultiProgress` status lines in the transcript (see Decision §6).
- Retention/cleanup of old report-bucket prefixes.

## Verification

- `mise run ci` clean (fmt-check + lint + test; 300 unit tests + 47 CLI tests passing).
- `cargo clippy --all-targets --all-features -- -D warnings` clean.
- Unit tests: `generate_run_id`/`run_prefix`/`write_summary_report` (`report_upload.rs`), `Transcript::line`/`create` (`observability/transcript.rs`).
- CLI tests: every `job run <type> --help` now lists `--report-bucket` (extended the existing per-job help-flag tests).
- Manual verification of an end-to-end upload against a real bucket-config is left to the first real run, consistent with this codebase's existing pattern for upload-phase changes (e.g. ADR-0089/ADR-0091 were verified the same way).
