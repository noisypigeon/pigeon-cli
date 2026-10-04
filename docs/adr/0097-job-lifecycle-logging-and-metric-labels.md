# ADR-0097: job lifecycle log events and instance/bucket metric labels

- **Author**: Willow Finch ([@noisypigeon](https://github.com/noisypigeon)).
- **Date**: 2026-10-04.
- **Status**: Accepted.

## Context

Debugging a live `job run deduplicate` invocation surfaced two real observability gaps, on top of one already filed as [issue #18](https://github.com/noisypigeon/pigeon-cli/issues/18) (from the ADR-0095 log analysis):

1. **No "job started" log row.** `run_instrumented` (`src/observability/mod.rs`) only logged once, after the wrapped closure returned (`"command finished"`). There was no equivalent line marking when the job began.
2. **A handled failure left no trace in `pigeon.jsonl`.** Every job wizard (`email_sync`, `email_pull`, `decrypt_files`, `pull_transform`, `deduplicate`, `reduce`) had an identical local `fn fail(message) -> i32 { eprintln!("Error: {message}"); FAILURE_EXIT_CODE }`, called from roughly a dozen `Err` branches per wizard. None of them emitted a `tracing` event -- the reason for the failure only ever reached stderr, never the structured log. This is exactly issue #18, caught the first time as "an invocation exited code 1 with zero WARN/ERROR events."
3. **No metric could be filtered by instance (host) or by bucket.** `pigeon_job_phase_total`/`pigeon_job_macro_phase`, the `pigeon_resource_*` samples, `pigeon_command_*`, and the five `pigeon_upload_*` metrics carried no `instance` or bucket label at all. ADR-0073 solved the exact same multi-instance ambiguity on the *logging* side (that's what the `instance` span field is for), but metrics never got the equivalent treatment, so a shared Cockpit store scraping several workers couldn't tell their data apart, and no metric could answer "how is bucket X's job doing."

Panics already produce a structured exception log via the ADR-0073 panic hook (`src/observability/panic.rs`), confirmed to fire correctly for both main-thread and `tokio::spawn`ed-task panics -- no gap there, so this ADR doesn't touch it. The "exception logging" need in practice is entirely about item 2: a non-panic `Err` becoming a silent failure.

**Scope of "a job"**: `run_instrumented` wraps exactly one `pigeon job run <type>` or `pigeon keyring <cmd>` invocation end to end -- it's called exactly twice in the whole codebase (`src/commands/job/commands.rs`, `src/commands/keyring/commands.rs`), with no narrower per-phase instrumentation anywhere else. "Job begins"/"job ends" in this ADR means that same granularity.

## Decision

### 1. Log a "job started" row

`run_instrumented` now emits `tracing::info!("command started")` immediately after `span.enter()` and before `f()` runs. It inherits `command`/`instance` via the enclosing span, exactly like the existing `"command finished"` line.

### 2. Log the actual reason when a job fails (closes #18)

One shared function now lives in `src/commands/mod.rs`, next to `FAILURE_EXIT_CODE`:

```rust
pub(crate) fn fail(message: impl std::fmt::Display) -> i32 {
    tracing::error!(error = %message, "job failed");
    eprintln!("Error: {message}");
    FAILURE_EXIT_CODE
}
```

The 6 duplicate local copies (one per job wizard) are gone; every wizard now calls this shared one. Every call site keeps working unchanged (same signature, same return value) -- this is a dedup-and-instrument, not a control-flow change.

### 3. "Job ended" row -- unchanged, already correct

`run_instrumented`'s existing `"command finished"` line (`exit_code`, `elapsed_ms`) already fires on every non-panic return, success or failure: `fail()`'s returned `i32` bubbles back up through each wizard's `block_on` into `f()`'s return value. Combined with #1/#2, a run now always brackets with `"command started"` -> (on failure) `"job failed"` with reason -> `"command finished"` with exit code.

### 4. A cached `instance()` accessor

```rust
pub(crate) fn instance() -> &'static str {
    static INSTANCE: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    INSTANCE.get_or_init(|| sysinfo::System::host_name().unwrap_or_else(|| "unknown".to_string()))
}
```

Replaces the inline `sysinfo::System::host_name()` call that used to live only inside `run_instrumented`. Resolved once per process; every metrics call site added below reuses it instead of re-querying `sysinfo` per item -- real runs process tens of thousands of items.

### 5. An `instance` label on every custom metric

Added to `record_phase_count`/`set_macro_phase` (`src/observability/metrics.rs`), the four `pigeon_resource_*` gauges/counters (`src/observability/resources.rs`, previously fully unlabeled), the two command-level metrics in `run_instrumented`, and all five metrics in `src/commands/job/upload.rs`. Purely additive -- no new metric names, just a new label on each existing one.

### 6. Bucket labels where a bucket is already in scope -- no new plumbing required

- `record_phase`/`record_phase_count` gained a new `source_bucket: Option<&str>` parameter. `Some(bucket_config.alias.as_str())` at every call site inside `deduplicate/worker.rs`, `pull_transform/worker.rs`, and `reduce/worker.rs`'s `process_item`-style functions, where a source `BucketConfig` was already an in-scope parameter. `None` where no source bucket exists: `email_sync`/`email_pull` (IMAP source), and `deduplicate/dedup.rs`'s `place_and_report` (operates on already-downloaded local files, no `BucketConfig` parameter there). A `None` becomes the literal label value `"n/a"` (the `NO_BUCKET` constant) -- the label is always present in the metric's fixed label set, only its value is conditional.
- `upload.rs`'s five metrics gained `"destination_bucket" => bucket_config.alias.clone()`, unconditionally -- `bucket_config` there is always the remote/destination bucket and was already a parameter of `upload_one`.
- Uses `BucketConfig.alias` (the pigeon-configured short name), not the raw S3 bucket name -- matches ADR-0010's precedent that alias is the one human-facing bucket identifier used everywhere else in this codebase.

## Consequences

- `pigeon.jsonl` now brackets every command with a start/end pair, and a failure always carries its reason as a structured `tracing::error!` event -- closes #18.
- Every custom metric can be filtered/grouped by `instance` in Grafana/Cockpit, resolving the multi-worker ambiguity that already motivated ADR-0073's log-side `instance` field.
- `pigeon_job_phase_total` and the five `pigeon_upload_*` metrics can be filtered/grouped by bucket alias (e.g. "how is bucket X's run doing" becomes a real query).
- Metric cardinality grows modestly (instance x bucket are both small, bounded label sets), consistent with Prometheus label-cardinality guidance.
- `fail()` is now one shared function instead of six near-identical copies.
- No change to panic-based exception logging (ADR-0073); already correct.

## Out of scope

- `pigeon_resource_*` metrics get the new `instance` label but no bucket label -- they're process-wide resource samples, not scoped to a particular bucket operation.
- Whether Prometheus/Alloy's scrape-level `instance` label already disambiguates hosts on its own (likely, if remote-write relabeling is configured) isn't verified here. This ADR adds an explicit application-level `instance` label regardless, as a robustness measure independent of the scrape-side config -- the same reasoning ADR-0073 used to add its own log-side `instance` field rather than relying on log-shipper host tagging.

## Verification

- `mise run ci` clean (fmt-check + lint + test), run in isolation.
- Manual: `PIGEON_LOG_DIR=<dir> pigeon keyring list` shows `"command started"` immediately followed by `"command finished"` in `pigeon.jsonl`.
- Manual: `pigeon job run deduplicate --source-bucket does-not-exist ...` produces a `"job failed"` ERROR event carrying `error = "no bucket-config named 'does-not-exist'"`, immediately before the usual `"command finished"` (`exit_code: 1`) line.
- `grep -rn "fn fail" src/commands/job/*/wizard.rs` returns nothing.
- Compiler-verified: every `record_phase`/`record_phase_count`/metrics call site compiles against the new label parameters, confirming no call site was missed.
