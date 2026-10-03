# ADR-0092: a local Prometheus metrics endpoint, for Scaleway Cockpit dashboards

- **Author**: Willow Finch ([@noisypigeon](https://github.com/noisypigeon)).
- **Date**: 2026-10-02.
- **Status**: Accepted.

## Context

`pigeon-cli` runs unattended on a Scaleway compute instance (provisioned by
the sibling `noisypigeon` terraform repo's `modules/scaleway/compute-instance`
module). Its only observability output today is the durable JSONL log
(`pigeon.jsonl`, ADR-0073) — structured `tracing` events, including a
periodic `ResourceSampler` that already computes CPU/RSS/disk-I/O numbers
(`src/observability/resources.rs`) but only ever writes them as JSON log
lines, never as a real, queryable time series. ADR-0073 explicitly rejected
OpenTelemetry/remote export at the time as "wrong shape for a single-run
CLI — no scrape endpoint or remote export is needed."

The user wants to build Grafana dashboards on top of pigeon-cli job runs,
via Scaleway's managed Cockpit product (Grafana + Loki + Mimir). That
revisits ADR-0073's non-goal directly: a scrape endpoint is now exactly
what's needed, because the terraform side (tracked separately in the
`noisypigeon` repo) will run an on-host Grafana Alloy agent that scrapes
Prometheus metrics and tails `pigeon.jsonl`, then forwards both to Cockpit.
This ADR covers only the pigeon-cli side of that split: emitting real
metrics, not shipping them anywhere — shipping is the terraform-side
agent's job, not this binary's.

## Decision

### A process-lifetime local Prometheus endpoint, not a job-scoped one

Add `metrics` (the facade macros: `counter!`/`gauge!`/`histogram!`) and
`metrics-exporter-prometheus` (a pull-based local HTTP `/metrics` renderer)
as new dependencies. `src/observability/metrics.rs::install(port)` installs
the global recorder once, in `main.rs`, right alongside
`observability::init()` — for every command, not scoped to job commands the
way `ResourceSampler::spawn` is. A keyring command binding an unused
listener for a few milliseconds costs nothing, and this avoids threading a
port through six separate job-wizard call sites just to match
`ResourceSampler`'s narrower scope. Binding failure (e.g. two `pigeon`
invocations racing for the same port) is non-fatal — warn and continue, the
same resilience posture as ADR-0068's best-effort IMAP logout. Controlled by
two new global flags, `--metrics-port` (default `9091`) and `--no-metrics`,
plus a `PIGEON_METRICS_PORT` env fallback mirroring `PIGEON_LOG_DIR`'s
precedent (ADR-0073).

The endpoint only exists while a `pigeon` process is running — an external
scraper will see it go down between invocations. That's expected, not a
bug; dashboards built on this data should not alert on bare `up == 0`.

### Dual-emit, don't replace, the existing JSON log lines

`ResourceSampler`'s loop and `run_instrumented()`'s per-command summary
(`src/observability/mod.rs`) now emit both the existing `tracing::info!`
event and an equivalent `metrics` call, at the same call site:

- CPU%, RSS, and cumulative disk read/write bytes become
  `pigeon_resource_cpu_percent`/`pigeon_resource_mem_bytes` gauges and
  `pigeon_resource_disk_{read,written}_bytes_total` counters.
  `sysinfo::Process::disk_usage()`'s totals are already
  cumulative-since-process-start, so these use `Counter::absolute()`, not
  `.increment()`, to stay monotonic without double-counting between
  samples.
- Every command dispatch (`run_instrumented`) becomes a
  `pigeon_command_duration_seconds` histogram and a
  `pigeon_command_runs_total{command, status}` counter (`status` is
  `"success"`/`"failure"`, derived from the exit code).
- Upload attempts (`commands/job/upload.rs::upload_one`, the one upload path
  all five upload-capable jobs share per ADR-0091) become
  `pigeon_upload_duration_seconds`, `pigeon_upload_bytes_total` (successful
  uploads only), and `pigeon_upload_failures_total`.

ADR-0078's log-analysis workflow still depends on the JSON lines, so none of
the existing `tracing::info!`/`warn!` calls were removed or altered beyond
this.

### Per-job summary counters, deliberately capped at per-job totals

Every job's final summary struct converges on the same shape — a
job-specific "processed" count (`placed`/`processed`/`synced`) plus
`failed`/`uploaded`/`unchanged`/`upload_failed` — even though each job's
`FailureBreakdown` has its own, non-overlapping category fields (sort's
`download`/`placement` vs. email-sync's
`connect`/`examine`/`batch_error`/`verification`/`parse_skipped`/
`missing_file`, etc. — five different vocabularies, ADR-0033). Rather than
invent a shared category taxonomy that doesn't actually exist underneath,
`observability::metrics::record_job_summary(job, processed, failed,
uploaded, unchanged, upload_failed)` is called once at each of the five
upload-capable jobs' wizard summary-print sites
(`email_sync`/`email_pull`/`pull_transform`/`dedupe`/`sort`'s `wizard.rs`),
emitting `pigeon_job_{items_processed,items_failed,uploaded,unchanged,
upload_failed}_total{job}` counters labeled by job name. `decrypt_files`
(the one job with no upload phase and no `FailureBreakdown`, ADR-0028) is
not wired up — it doesn't share this shape and its summary is simple enough
that the JSON log alone still covers it.

This is a first-iteration simplification, not a permanent ceiling — see Out
of scope.

## Consequences

- New runtime dependency surface (`metrics`, `metrics-exporter-prometheus`,
  and their transitive deps — notably `quanta`, `hashbrown`, `rand`), the
  first metrics/observability-export-adjacent crates this repo has taken on
  since ADR-0073 deliberately avoided them.
- A `pigeon` process now opens a localhost TCP listener for every
  invocation unless `--no-metrics` is passed — new attack/footprint surface
  to be aware of, though scoped to loopback only (`127.0.0.1`, never
  `0.0.0.0`).
- Dashboards can now be built against real counters/gauges/histograms
  instead of parsing JSON log lines for numbers — the actual goal of this
  change.

## Out of scope

- Shipping these metrics anywhere — that's the on-host Grafana Alloy
  agent's job, tracked in the `noisypigeon` terraform repo, not this ADR.
- A shared `FailureBreakdown` category taxonomy / per-category metric
  labels across jobs — deferred until the current per-job-total cut proves
  too coarse for an actual dashboard need.
- Wiring `decrypt_files` into `record_job_summary` — it doesn't share the
  five upload-capable jobs' summary shape.
- A `run_id` field to correlate one invocation's metrics window with its
  log lines when two runs happen back-to-back on the same host — the same
  gap ADR-0078 already flagged for log analysis alone; worth its own ADR if
  it turns out to matter once real dashboards are built.
