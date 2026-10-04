# ADR-0093: continuous job-progress metrics; logs become diagnostics-only

- **Author**: Willow Graysen ([@noisypigeon](https://github.com/noisypigeon)).
- **Date**: 2026-10-03.
- **Status**: Accepted.

## Context

ADR-0092 gave pigeon-cli a local Prometheus endpoint, but almost everything
on it only gets recorded **once, at the end** of a job: `record_job_summary()`
(per-job totals) and `run_instrumented()`'s command duration/status are both
called after the whole job returns. The only thing that updates *during* a
run is `ResourceSampler` (every 5s) and upload-phase metrics, which stay at
zero until the upload phase starts (ADR-0019: upload never begins until all
local work is done). This was confirmed live on a real test-bed instance:
mid-run, Cockpit only showed the 4 resource gauges.

The user also flagged, from a live Cockpit Logs screenshot, that
`resource_sample` lines are pure periodic telemetry, not log-worthy — logs
should be reserved for errors/failures/"what happened and why," and should
carry **job** and **instance** identity so a shared, multi-instance Cockpit
store (tracked separately in the `noisypigeon` terraform repo) stays
filterable.

A research pass across all 5 upload-capable jobs' `worker.rs` files found
every per-item success/failure branch point already in the code, confirmed
`metrics`' macros are safe to call from every concurrency shape already in
use (`stream::buffer_unordered`, `tokio::spawn` workers over a shared queue,
and sequential post-join passes), and surfaced two real pre-existing gaps,
fixed as a side effect of touching these exact lines:

- **`dedupe`'s placement failures were logged but never counted anywhere** —
  `dedup.rs`'s placement loop `continue`d on a failure without touching any
  counter at all, and `dedupe`'s own `FailureBreakdown`/`PlacementSummary`
  didn't even have a field for it (unlike `pull_transform`'s equivalent,
  which already tracked this correctly). Both structs now have a
  `placement`/`failed` field, wired the same way `pull_transform` already
  does it.
- **`pull_transform`'s `FailureBreakdown.recode` field was declared but
  never incremented** — a failed recode falls back to the original file
  (tracked via a separate `fallback_count`) rather than failing the item.
  The new metric reflects this directly (`outcome="recoded"|"fallback"`),
  rather than forcing a success/failure binary that doesn't match reality.

**Naming note**: once metrics from multiple instances land in one shared
Mimir store (terraform-side work, out of scope here), Prometheus's own
reserved `job` label — which the scrape config uses to identify the *scrape
target* — would collide with a label meaning "which pigeon job type."
Every "which job" label introduced below is `pigeon_job`, not `job`.

## Decision

### A live per-item counter, replacing the once-at-the-end job totals

```
pigeon_job_phase_total{pigeon_job, phase, outcome}
```
`phase` reuses each job's existing `FailureBreakdown` field names
(`"download"`, `"placement"`, `"hash"`, `"archive"`, `"classify"`,
`"connect"`, `"examine"`, `"batch_error"`, `"verification"`,
`"parse_skipped"`, `"missing_file"`, `"recode"`, `"attachment_extraction"`);
`outcome` matches each phase's real semantics rather than a forced
success/failure binary (`"ok"`/`"failed"` where that fits,
`"recoded"`/`"fallback"` for `pull_transform`'s recode step,
`"extracted"`/`"extraction_failed"` for `email_pull`'s attachment step
since that still counts as `synced` in the job's own model,
`"skipped_type"` for a `pull_transform` item excluded by `--file-types`).

Added inline at every per-item completion point already in the code (one
counter call next to each existing success/failure branch — `sort`,
`dedupe`, `pull_transform`'s concurrent worker-pool/`buffer_unordered`
loops and their sequential placement passes; `email_sync`/`email_pull`'s
per-batch connect/examine/dispatch failures via the new
`record_phase_count` for a batch's whole `uid_count` at once, and their
per-UID missing-file/verify/parse-skip/attachment-extraction outcomes via
`record_phase`).

**Retired**: `record_job_summary()` and the once-at-the-end
`pigeon_job_items_processed_total`/`_failed_total`/`_uploaded_total`/
`_unchanged_total`/`_upload_failed_total` counters from ADR-0092 — fully
superseded by `sum by (pigeon_job, outcome) (pigeon_job_phase_total{...})`;
keeping both would double-count the same events under two names. The 5
wizard.rs call sites added in ADR-0092 are removed.

### A coarse "what phase is the job in" gauge

The pipeline is genuinely concurrent — many items are in different phases
simultaneously, so a single "current phase" gauge would misrepresent that
(the per-phase *rate* of the counter above already answers fine-grained
"what's happening now"). The one real sequential boundary every job has is
local-work-then-upload (ADR-0019: upload never starts until local work is
fully done). Added:
```
pigeon_job_macro_phase{pigeon_job}   # 0 = local work, 1 = uploading
```
Set to `0` once at the top of each job's normal-run entry point
(`run_sort_job`, `run_dedupe_job`, `run_pull_transform_job`,
`run_email_sync_job`, `run_email_pull_job`), and to `1` in
`upload.rs::run_upload_phase` — the single choke point all 5 jobs' upload
phases (7 call sites, including each job's `--upload-only` path) funnel
through, deriving the job name from the already-threaded `UploadTask.job`
field (see below) rather than needing its own parameter at every call site.

### Job-labeled, richer upload metrics

`UploadTask` had no job field — `pending_upload_tasks()` is the single
construction point all 5 jobs' own `upload_result()`/`identity_upload_tasks()`
wrappers funnel through (10 call sites: normal run + `--upload-only`, per
job). Threaded a `job: &'static str` parameter through
`pending_upload_tasks`/`UploadTask`, each of the 10 call sites passing its
own static job name. Then:
- `pigeon_upload_bytes_total`/`pigeon_upload_duration_seconds` gain a
  `pigeon_job` label.
- `pigeon_upload_failures_total` is replaced by
  `pigeon_upload_outcomes_total{pigeon_job, outcome="uploaded"|"unchanged"|"failed"}`
  (matching `upload_one`'s existing three-way match exactly — one metric,
  not three).
- New `pigeon_upload_attempts_total{pigeon_job}`, incremented where the
  `"upload started"` log line used to fire (see below) — a live
  "uploads in flight" signal without needing a log line for it.

### Logs: diagnostics only, structured job/instance context

Removed two telemetry-shaped log lines — metrics now cover what they
reported, and they were the only two found anywhere in the job tree that
fit this shape (every other `warn!`/`error!` across 50 audited sites
already names a specific failure/panic with an `error = %err`-style
reason, and stays exactly as-is):
- `src/observability/resources.rs` — `tracing::info!(kind =
  "resource_sample", ...)`, which fired every 5s for a run's entire
  lifetime; the equivalent gauges/counters from ADR-0092 already cover it.
- `src/commands/job/upload.rs` — `tracing::info!(file = ..., bytes = ...,
  "upload started")`, replaced by `pigeon_upload_attempts_total`.

**Job + instance on every log line**: `run_instrumented()` already opens a
`command`-named span with `command = command_name` (= "what job," already
inherited by every event via `spans[]`). Added a second field, `instance`,
resolved once per process via `sysinfo::System::host_name()` — no new
dependency, `sysinfo` is already used by `ResourceSampler`:
```rust
let instance = sysinfo::System::host_name().unwrap_or_else(|| "unknown".to_string());
let span = tracing::info_span!("command", command = command_name, instance = %instance);
```
"What happened" is already `fields.message` on every event — no schema
change needed there.

## Consequences

- Every job now has real, incrementally-updating progress visibility on the
  Prometheus endpoint while it's running, not just a final tally — the
  actual goal of this change.
- `dedupe`'s placement-failure count (previously silently absent from
  every summary/metric) and its exit-code-driving `failed` total are now
  correct; a `dedupe` run with placement failures that previously reported
  success (exit code 0) now correctly reports failure.
- `pigeon.jsonl` loses its highest-volume, lowest-value log line
  (`resource_sample`, previously one line per 5s for a run's whole
  lifetime) and `upload.rs`'s `"upload started"` event. The
  `analyze-job-run` skill (ADR-0078) was built entirely around both —
  "the log is often the *only* evidence available" for OOM/crash
  forensics specifically relied on `resource_sample`'s climbing
  `mem_bytes`, and the absence-of-a-matching-completion trick on
  `"upload started"` to name the in-flight file at crash time. Both
  techniques only work now if Cockpit/Alloy was actually deployed and
  scraping the Prometheus endpoint on that machine — on one that wasn't,
  this data is simply gone once the process exits, a real capability
  regression (not just a relocation), confirmed and accepted by the user
  before implementing. The skill is rewritten in this same PR (its step 4,
  "Known limitations", and cheat-sheet sections) to point at the
  Prometheus/Cockpit metrics instead and state the gap explicitly rather
  than let the documented procedure silently go stale.
- Metrics gain a `pigeon_job` label distinct from Prometheus's own reserved
  `job` label, a naming discipline that needs to be remembered for any
  future metric added to this crate.

## Out of scope

- Shipping/forwarding these metrics or the still-diagnostic logs anywhere
  — that's the on-host Grafana Alloy agent's job, tracked in the
  `noisypigeon` terraform repo (a separate ADR covers moving to one shared
  Cockpit store there, now that logs/metrics carry job/instance identity).
- `pigeon_command_duration_seconds`/`pigeon_command_runs_total` — inherently
  end-of-run (duration isn't known until the command finishes), not part
  of the "job progress" ask.
- A finer-grained single-phase gauge than local-work/upload — rejected as
  a poor fit for the genuinely concurrent pipeline (see above).
- Per-identity metric/log labels (e.g. which email identity) — cardinality
  risk, not asked for.
