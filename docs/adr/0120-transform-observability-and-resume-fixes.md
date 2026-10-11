# ADR-0120: `transform`'s missing transcode metric, child-process-blind resource sampling, and its ephemeral-VM-incompatible resume checkpoint

- **Author**: Willow Graysen ([@noisypigeon](https://github.com/noisypigeon)).
- **Date**: 2026-10-10.
- **Status**: Proposed.

## Context

A real `pigeon job run transform --input-file-type=png --source-path ... --destination-path ...` run (11,731 PNGs pulled, 11,729 pushed successfully) left behind a session transcript and a Grafana `pigeon-cli overview` dashboard snapshot. The 2 failures in that run are genuine bad source data, correctly isolated by ADR-0116's per-file handling (every other file still transcoded and pushed) -- no fix needed for them:

- `2022-10-25_Return_your_Mack_Weldon_items_to_our_store_1.png` -- ffmpeg: `chunk too big` / `Invalid data found when processing input` (a truncated/corrupt PNG).
- `IMG_1959 1.PNG` -- the post-encode dimension guard (`media.rs::transcode_to_jpg`, added to catch exactly this) rejected an output that came back `(0, 0)` instead of the source's `(1179, 2556)`.

Cross-referencing the dashboard against an `htop` screenshot taken on the job's VM during the same run, and the job's own source, surfaced three real gaps that do need fixing.

### 1. No `transcode` phase in the "Job phases by phase & instance" panel

That panel queries `sum by (phase, instance) (increase(pigeon_job_phase_total{...}[$__rate_interval]))` -- it already groups by whatever `phase` label values exist, so no dashboard change is needed. The bug is that the transcode/copy-through step never calls `record_phase_count` at all. `rclone_transfer.rs` records `phase="pull"` for Phase A; `transform/push.rs` records `phase="push"` per file; nothing records a `transcode` phase, despite ADR-0112 §9 originally specifying exactly this. The dashboard legend showing only `pull`/`push` is a direct, accurate reflection of that gap.

### 2. CPU/Memory panels only reflect the `pigeon` process itself

Grafana showed ~2.5% CPU and ~24 MiB memory for the run's instance while `htop` on that same VM, at the same moment, showed 8 cores pegged near 100% by several concurrently-running `ffmpeg` processes. `ResourceSampler::spawn` (`src/observability/resources.rs`) samples exactly one PID -- `sysinfo::get_current_pid()`, the `pigeon` process itself -- via `refresh_processes(ProcessesToUpdate::Some(&[pid]), true)`. `ffmpeg` (spawned by `transform/media.rs`, `pull_transform/media.rs`) and `rclone` (spawned by `rclone_transfer.rs`, `transform/push.rs`) are separate child processes via `tokio::process::Command`, invisible to a single-PID query. `ResourceSampler::spawn` is shared by all 7 job types (9 call sites across their `wizard.rs` files), so this undercounts CPU/memory/disk I/O for every job that shells out to a subprocess, not just `transform`.

### 3. The pre-run directory scan and checkpoint are local-disk-only, but `transform` runs on ephemeral VMs that never persist local disk between runs

`transform/manifest.rs`'s `load_checkpoint` (reads `<local_output>/.staging/.processed`) and `gather_pending` (walks `<local_output>/source/`) are both scoped entirely to local disk. The session transcript confirms the job runs on a freshly-provisioned cloud VM per invocation (`ssh -J ... pigeon-cli-rneykw-transform-png-to-jpg...internal`, a brand-new Ubuntu instance) -- local disk never survives a VM replacement. Both mechanisms therefore always find nothing on a genuinely fresh VM, silently defeating "resume an interrupted run" in production: every VM replacement after a crash re-downloads, re-transcodes (the expensive step), and re-pushes every file from scratch, even files already durably pushed to `--destination-path` in a prior attempt.

What actually persists across runs is `--destination-path` itself. `placement::compute_destination_name(source_path, relative_path)` already makes "is this file done" a deterministic, checkable question against it: the same source file maps to the exact same destination filename on every run (ADR-0112 §5), so a listing of what's already at the destination can answer "already done" without any local state at all.

## Decision

### 1. Record a `transcode` phase metric per file

`process_one` (`src/commands/job/transform/worker.rs`) captures the transcode/copy-through result in a local binding instead of immediately propagating it with `?`, calls `record_phase_count("transform", "transcode", outcome, 1, None)` at that point -- mirroring `push.rs::push_one`'s existing pattern of recording right where the step resolves, not deferred to the dispatch loop -- then propagates:

```rust
let transcode_result = match input_file_type {
    InputFileType::Jpeg => media::copy_through(&pending_file.absolute_path, &scratch_path),
    InputFileType::Png | InputFileType::Heic => {
        retry_with_backoff(TRANSCODE_RETRIES, TRANSCODE_RETRY_BACKOFF, || {
            media::transcode_to_jpg(&pending_file.absolute_path, &scratch_path)
        })
        .await
    }
};
crate::observability::metrics::record_phase_count(
    "transform",
    "transcode",
    if transcode_result.is_ok() {
        outcome_for(input_file_type).as_str()
    } else {
        Outcome::Failed.as_str()
    },
    1,
    None,
);
transcode_result?;
```

`outcome_for`/`Outcome::as_str()` already exist in `worker.rs` and return exactly `"transcoded"`/`"copied_through"`/`"failed"` -- no new type needed.

### 2. `ResourceSampler` sums CPU/memory/disk I/O across the whole process tree

`src/observability/resources.rs`, `ResourceSampler::spawn`:

- `refresh_processes(ProcessesToUpdate::Some(&[pid]), true)` becomes `refresh_processes(ProcessesToUpdate::All, true)`.
- A new helper walks `Process::parent()` to a fixed point, finding every live descendant of the root `pigeon` PID (direct children like `ffmpeg`/`rclone`, and any children of theirs):

  ```rust
  fn pigeon_process_tree(system: &System, root: sysinfo::Pid) -> Vec<&sysinfo::Process> {
      let mut ids = std::collections::HashSet::from([root]);
      loop {
          let before = ids.len();
          for (pid, process) in system.processes() {
              if !ids.contains(pid) && process.parent().is_some_and(|p| ids.contains(&p)) {
                  ids.insert(*pid);
              }
          }
          if ids.len() == before {
              break;
          }
      }
      ids.into_iter().filter_map(|pid| system.process(pid)).collect()
  }
  ```

- `cpu_usage()`/`memory()` are summed across the returned processes into the existing `pigeon_resource_cpu_percent`/`pigeon_resource_mem_bytes` gauges.
- Disk I/O switches from `DiskUsage::total_read_bytes`/`total_written_bytes` (applied via `.absolute()`) to `DiskUsage::read_bytes`/`written_bytes` -- `sysinfo`'s own "since the last refresh" per-process deltas -- summed across the tree and applied via `.increment()`. This is a necessary part of the fix, not cosmetic: summing *cumulative* per-process counters with `.absolute()` across a tree whose membership changes every tick (a short-lived `ffmpeg` process exits, another starts) would let the gauge plateau or double-count; summing each tick's own deltas and incrementing stays correct regardless of which processes existed at the last tick.
- A multi-core sum can legitimately read above 100% CPU during heavy parallel `ffmpeg` use -- confirmed real in the `htop` screenshot (8 cores near 100% simultaneously) -- this is correct behavior, not something to clamp.
- No change needed at any of the 9 `ResourceSampler::spawn` call sites; all 7 job types benefit automatically. No `overview.json` dashboard changes needed -- the CPU/Memory panels already query by metric name only, with no fixed `max` on the percent unit.

### 3. Destination-path listing replaces the local `.processed` checkpoint

New file `src/commands/job/transform/destination.rs`:

```rust
pub(crate) async fn list_existing_filenames(destination_path: &str) -> Result<HashSet<String>, String>
```

Shells out to `rclone lsjson --files-only --no-modtime --no-mimetype <destination_path>`, following `push.rs`/`rclone_transfer.rs`'s existing subprocess/error-formatting conventions, wrapped in `retry_with_backoff` (the same 3-retries/2s-backoff shape as `push.rs`'s `PUSH_RETRIES`/`PUSH_RETRY_BACKOFF`). Parses stdout as JSON into entries carrying a `Name` field, collected into a `HashSet<String>`. A destination that doesn't exist yet (the very first run against a brand-new path) resolves to `Ok(HashSet::new())`, not an error -- rclone's "directory not found" failure is detected and treated as empty; any other non-zero exit is a real `Err`.

`transform/manifest.rs`: `load_checkpoint`, `append_checkpoint`, and `PROCESSED_FILE_NAME` are deleted outright, along with their three dedicated tests -- proven dead weight on this job's real ephemeral infrastructure, not kept as a fallback. `gather_pending` drops its `staging_dir` parameter and all checkpoint filtering, becoming a plain "walk `source_dir`, filter by extension" lister.

`transform/worker.rs`: `enqueue`'s signature changes from checking a local `done_checkpoint: &HashSet<String>` to computing `placement::compute_destination_name(source_path, &relative_path)` and checking it against `existing_destination_filenames: &HashSet<String>` (sourced from `destination::list_existing_filenames`), ahead of the existing `dispatched` double-dispatch guard, which is unchanged. `run_transform_job` calls `destination::list_existing_filenames(&plan.destination_path)` once before the pull subprocess spawns -- the same point the old local scan ran -- and both the initial local-scan dispatch source and the live-tail dispatch source route through the same updated `enqueue`. The dispatch loop's success arm drops the `append_checkpoint` call entirely. `placement::place_one`'s hard-error-on-collision backstop is untouched, remaining a second, independent safety net against any double-dispatch that slips through.

**Accepted limitation, stated explicitly**: Phase A still re-pulls bytes for already-done files on every rerun -- the destination's hash-based naming is one-way, so a pull-side `--exclude` filter isn't feasible without inverting it. This fix removes the expensive re-transcode and re-push for already-done files, which is the real cost this incident traces; it does not remove redundant re-download.

## Consequences

- The dashboard's "Job phases by phase & instance" panel now shows `transcode` alongside `pull`/`push`.
- The CPU/Memory panels reflect true utilization including `ffmpeg`/`rclone` children, for every job type, not only `transform`. Readings can legitimately exceed 100% CPU during multi-core transcoding -- expected, not a defect.
- A `transform` rerun after VM loss now correctly skips already-fully-pushed files' transcode and push, instead of silently redoing all of it.
- A short-lived `ffmpeg`/`rclone` child that starts and fully exits between two 5-second sampler ticks still contributes nothing to that tick's metrics -- an irreducible point-sampling limitation, not something this ADR claims to solve.

## Out of scope

- A pull-side `--exclude` filter driven by the destination listing (infeasible without inverting `compute_destination_name`'s hash).
- Any change to `pull_transform`'s own metrics/resource/resume design.
- Structured "skipped -- already at destination" accounting in `transform-report.txt`, or a new report/metric outcome for it.
- Shortening the 5-second sampler interval, or event-driven (non-polling) child-process tracking.
- Any `overview.json` dashboard changes -- confirmed unnecessary for all three fixes.

## Verification

- `mise run ci` clean (fmt-check + lint + test), including new/updated tests in `destination.rs`, `manifest.rs`, and `worker.rs` (see module-level test changes).
- Manual: `curl localhost:9091/metrics | grep pigeon_job_phase_total` during a `transform` run shows `phase="transcode"` entries.
- Manual: `htop` on the job's VM alongside a real multi-file transcode, cross-checked against the Grafana CPU/Memory panels for the same instance and time window.
- Manual: two sequential `transform` runs against the same `--destination-path`, deleting `--local-output` between them to simulate VM replacement -- the second run pushes 0 new files for content already at the destination and does not re-invoke `ffmpeg` for those files.
- Manually confirmed, before merge, the exact stderr text the installed `rclone` version emits for `lsjson` against a nonexistent destination, since pattern-matching subprocess output is inherently version-fragile.
