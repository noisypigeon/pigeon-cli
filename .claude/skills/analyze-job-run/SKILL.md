---
name: analyze-job-run
description: Use this skill when diagnosing a `pigeon` job run from its logs -- reading the ADR-0073 JSONL observability log (`pigeon.jsonl`) to reconstruct a run's timeline, spot failure patterns, and produce a written takeaway (root cause, evidence, confidence, next action); cross-referencing the ADR-0092/0093 Prometheus metrics endpoint (or its Cockpit-forwarded history, if deployed) for resource/progress data, now that the log itself is diagnostics-only. Trigger on requests like "can you check the logs", "what happened in the last run", "read the log dump and tell me what went wrong", "why did this job crash", or a pasted terminal transcript showing a job failure/crash/unexpected exit code.
---

# Analyzing a `pigeon` job run's logs

The ADR-0073 JSONL log is the durable record of *what went wrong and why*
during a run -- when a run is killed (SIGKILL, OOM) nothing else gets a
chance to log gracefully on the way out, so for diagnostic (error/failure)
events, the log is often the *only* evidence available. This skill
formalizes the ad hoc diagnostic process that first found the ADR-0076
SIGKILL root cause (a real crash against 50-100GB zip archives in the
since-removed `pull-transform` job), so it's repeatable instead of
re-derived from source every time.
Follow it in full; don't skip straight to guessing a cause.

**ADR-0093 changed what's *in* the log.** Resource telemetry
(`resource_sample`: CPU/mem/disk-I/O) and the `"upload started"` event were
both moved to Prometheus-only (`pigeon_resource_*`/`pigeon_upload_*`
metrics on the local `:9091/metrics` endpoint) and are **no longer in
`pigeon.jsonl` at all**. If a Grafana Alloy agent is forwarding that
endpoint to a Scaleway Cockpit instance (tracked in the `noisypigeon`
terraform repo), that history survives a crash and is queryable after the
fact the same way the log always was. If it isn't -- a local dev machine,
or any instance predating that terraform rollout -- **that data no longer
exists anywhere** once the process is gone; the log alone can no longer
answer "was memory climbing" or "which file was mid-upload when it died."
Say so explicitly in the takeaway rather than silently working around the
gap. See step 4 and the Known limitations section.

## Steps

1. **Locate the log.** Default path, **rotated daily since ADR-0151**:
   `~/Library/Application Support/pigeon/logs/pigeon.<YYYY-MM-DD>.jsonl`
   (macOS) or `~/.local/share/pigeon/logs/pigeon.<YYYY-MM-DD>.jsonl` (Linux -- `directories`'
   `data_local_dir()`, i.e. `$XDG_DATA_HOME` or `~/.local/share` by
   default; this is the path a headless Linux box running a long `job run
   deduplicate`/`transform` will actually have, ADR-0089). Overridable two
   ways -- check both before assuming the default: the `PIGEON_LOG_DIR`
   environment variable, or a `--log-file <path>` flag passed to the
   command itself. If the user pasted a terminal transcript, look for
   either of these before reading anything. 14 generations are kept; a run
   that straddled midnight has its lines split across two of them, so glob
   rather than naming one file. An **explicit `--log-file <path>` is not
   rotated** -- that path is written literally. A pre-ADR-0151
   `pigeon.jsonl` with no date may also still be sitting there; it is not
   migrated or removed, and holds everything before the rotation cutover.

   **If the symptom is a SIGKILL/crash with no graceful exit, check for an
   OOM kill before reading the JSONL at all**: `dmesg | grep -i oom` or
   `journalctl -k | grep -i oom` (Linux). The kernel's own OOM-killer log
   line (timestamp + `anon-rss:<kB>`) is faster to get to than reconstructing
   the same conclusion from `pigeon_resource_mem_bytes` climbing (step 4),
   and dates/correlates directly against the JSONL timeline from step 3 once
   you have it (ADR-0089 -- this is exactly how a real `deduplicate` OOM mid-upload
   was first confirmed, back when this signal still lived in the log itself).

2. **Isolate the run.** Each generation still accumulates lines from every
   run of every command, interleaved in chronological order.

   **Since ADR-0151 there is a `run_id`** on the outer `command` span, so
   isolating one invocation is a direct filter rather than a timestamp
   guess:
   ```
   jq 'select(.spans[]?.run_id == "18de49d4e714ce98-pigeon-5ece")' pigeon.*.jsonl
   ```
   It is the same id as that run's ADR-0100 artifact prefix
   (`{date}-{job}-{run_id}/`), so a report-bucket object name hands you the
   filter directly. Caveat: lines emitted *before* the job's wizard
   generates the id -- notably `"command started"` -- carry no `run_id`, so
   bound those by timestamp as below. A pre-ADR-0151 log has the field on no
   line at all.

   The two older signals still apply, and are what you need on such a log:
   - The outer span's `command` field identifies *which command*, not
     *which invocation* -- filter to it first:
     ```
     jq 'select(.spans[]?.command == "job.transform")' pigeon.jsonl
     ```
     (swap in `job.email-import`, `job.deduplicate`, `keyring.add`,
     `keyring.modify`, `keyring.delete`, or `keyring.list` as needed --
     these are the exact `command_name()` strings, see the cheat sheet
     below.)

     Before ADR-0098, `deduplicate`'s archive/download/hash
     warnings -- logged from inside `tokio::spawn`ed worker tasks and
     `tokio::task::spawn_blocking` closures -- were missing this `command`
     span entirely (neither primitive propagates the caller's ambient span
     on its own), so this filter silently missed them. If you're looking at
     a `pigeon.jsonl` from before that fix, don't trust an empty result from
     this filter as proof nothing archive-related happened; cross-check by
     grepping the raw file for `"step":"archive"` with no `spans[]` filter
     at all.
   - Within that filtered stream, bound by **timestamp proximity** to
     what the user reported (a specific time, "just now", "the last run").
     A crashed run's last line for that `command` will simply be the last
     line in the file for it, with **no matching `"command finished"`
     event** (paired with a `"close"` event for the same span) after it.
     That absence *is* the crash signature -- SIGKILL can't be caught, so
     there was never a chance to log a graceful exit. Don't go looking for
     an explicit "crashed" log line; there won't be one.
   - This approach is fragile if two invocations of the *same* command
     ran concurrently (nothing disambiguates them) -- see Known
     limitations.

3. **Establish the timeline.** Within the isolated window, read events in
   timestamp order. Note the first timestamp (run start) and the last
   (run end, or the point it stopped if it crashed). If a
   `"command finished"` event is present, its `fields.exit_code` and
   `fields.elapsed_ms` tell you the outcome and duration directly.

4. **Pull resource/progress data from Prometheus, not the log** (ADR-0093 --
   `resource_sample` and `"upload started"` no longer exist in
   `pigeon.jsonl`; this is now `pigeon_resource_*`/`pigeon_upload_*`/
   `pigeon_job_phase_total`/`pigeon_job_macro_phase` on the process's local
   `:9091/metrics` endpoint while it's running, or their scraped history in
   Scaleway Cockpit's Mimir store if a Grafana Alloy agent was forwarding
   it). **If Cockpit isn't deployed on this machine, this data is simply
   gone once the process exits -- say so explicitly rather than searching
   the JSONL for it.** If it is:
   - **`pigeon_resource_mem_bytes` climbing without bound** -> memory
     pressure/a leak/an unbounded in-memory buffer.
   - **`pigeon_resource_disk_read_bytes_total` far exceeding any known
     input size** for the run (e.g. gigabytes of "read" against a bucket
     whose objects are only megabytes) -> the OS-level swap-thrashing
     signature that confirmed ADR-0076's root cause -- the process wasn't
     really reading that much real data, it was being paged in and out
     under memory pressure.
   - **The scrape target (`up{...}`) going down, or `pigeon_job_phase_total`/
     `pigeon_upload_bytes_total` samples stopping abruptly** well before
     any expected completion -> the process died at roughly that timestamp
     (cross-check against step 2's "no closing event" finding in the log).
   - **`pigeon_job_macro_phase`'s last value before samples stop** tells you
     whether the process died during local work (`0`) or mid-upload (`1`),
     the same thing the old `"upload started"`-absence trick used to answer
     from the log alone.

5. **Pull WARN/ERROR events, group by `step`**:
   ```
   jq 'select(.level=="WARN" or .level=="ERROR")' pigeon.jsonl
   ```
   Most of these carry a `fields.step` value naming which phase failed
   (see the cheat sheet's per-job vocabulary below) plus `fields.error`
   (the error string) and an identifying field (`key`, `uid`, `identity`,
   `mailbox`, `file` -- whichever applies). Group/count by `step` and
   **cross-check the tally against the job's own printed summary line**
   (every job prints one on completion -- exact formats below) to confirm
   the log and the terminal output agree. A mismatch is itself a finding
   worth calling out, not something to silently paper over.

6. **Check for a panic**:
   ```
   jq 'select(.fields.message == "panic")' pigeon.jsonl
   ```
   Fields: `panic` (the panic message plus file:line), `backtrace` (full
   stack), `span_trace` (the active span stack at panic time). If present,
   this is close to a smoking gun -- quote it directly in the takeaway.

7. **Write the takeaway.** Use this fixed shape, not free-form prose:
   - **Summary** -- one paragraph, plain English, what happened.
   - **Root cause** -- a hypothesis with an explicit confidence level
     (e.g. "confirmed", "likely", "possible, needs more data"). Never
     state a cause the log doesn't actually support -- if the evidence is
     inconclusive, say so plainly and name what additional data (a rerun
     with `--log-level debug`, a longer log window, the exact command
     line used) would resolve it, rather than guessing.
   - **Evidence** -- quote the specific log lines/timestamps/fields relied
     on. A reader should be able to verify every claim against the
     `jq` output themselves.
   - **Next action** -- concrete: fix a specific bug, write a follow-up
     ADR, rerun with different flags, or ask the user a specific question.
     If the takeaway reveals an actionable design gap, offer to write a
     new ADR proposing a fix -- this is exactly how ADR-0076 (streaming
     downloads/zip expansion to fix a real memory-exhaustion SIGKILL)
     began: a log-reading session that surfaced a root cause worth fixing.

## Reference: field and vocabulary cheat sheet

Reflects the schema as of ADR-0073/0074/0075/0076/0077/0089/0092/0093
(2026-10-03). If a
filter below unexpectedly returns nothing for a run that should have
matching lines, the schema may have drifted -- fall back to
`jq 'select(.target | startswith("pigeon::"))'` to see everything, or
re-grep the relevant job's source for new `tracing::` call sites. **Update
this cheat sheet in the same PR that changes the logging code** -- don't
let it go stale silently.

**Top-level JSONL fields on every line:**
- `timestamp` -- RFC3339 with microseconds.
- `level` -- `INFO` / `WARN` / `ERROR` / `DEBUG` / `TRACE`.
- `fields` -- object; always has `message`, plus every named field passed
  to the `tracing::info!`/`warn!`/`error!` call or `#[instrument(fields(...))]`.
- `target` -- the Rust module path of the call site (e.g.
  `pigeon::commands::job::transform::worker`).
- `span` -- the *innermost* currently-active span (name + its own fields).
- `spans` -- the full ancestor span stack, outermost to innermost, same
  shape as `span` per entry. Note: on a normal event, `spans` includes the
  span itself; on the auto-emitted `"close"` event for a span, `spans` is
  empty while `span` still names the closing span.

**`command_name()` values** (the `command` field on the outermost
`"command"` span -- this is what step 2's filter matches on):
`job.email-import`, `job.deduplicate`, `job.transform`,
`job.rclone-copy`, `job.rclone-delete`, `keyring.add`,
`keyring.modify`, `keyring.delete`, `keyring.list`.

**Keyring commands emit no per-operation tracing at all** -- only the two
boilerplate `"command finished"`/`"close"` events for the outer span. A
keyring-specific issue (a bad alias, a keychain prompt, a validation
failure) is **not diagnosable from this log** -- it only ever went to
stderr via `eprintln!`. Don't spend time searching the JSONL for it; ask
the user for the terminal output instead.

**Per-job `step` vocabulary** (WARN/ERROR events' `fields.step`):
- `email-import`: `connect`, `examine`, `fetch`, `extract`, `push`,
  `missing_file`, `gate`. No `upload` step -- delivery is its own per-object
  `rclone copyto` (ADR-0146 §5), and there is no separate upload phase to
  name. A `gate` event means an endpoint was refusing work and the run
  paused rather than failed (ADR-0146 §4); `paused_ms` on the matching
  resume line is how long that cost.
- `transform`: `pull`, `transcode`, `push` -- note this job does **not**
  share the generic `upload` step, since its push leg is its own per-file
  `rclone copyto` (ADR-0116) rather than the shared upload phase ADR-0147
  deleted. Before ADR-0130 this job
  emitted no `tracing::info!` at all, so a run from an older build has no
  phase-boundary or totals lines in the JSONL whatsoever -- only WARN/ERROR.
  From ADR-0130 on, look for `"transform: pull phase starting"`,
  `"transform: pull phase complete"`, and `"transform: run complete"`
  (which carries the full per-stage breakdown: `failed_pull`,
  `failed_transcode`, `failed_place`, `failed_push`, plus
  `skipped_already_done`, `skipped_apple_double`, and -- since ADR-0151 --
  `skipped_lock_file` for Excel `~$` stubs, which ADR-0149 had folded into
  the AppleDouble count on one path and dropped entirely on the other,
  and -- since ADR-0157 -- `skipped_email_shaped` for `md` files recognized
  as rendered `.eml` messages, which were pulled but deliberately not
  converted; it is also on the 60 s `"transform: progress"` line, and each
  such file is a `skipped_email_shaped` row in `transform-report.txt`
  naming the signal families that matched).
  A `"rclone's pull error count disagrees with the number of undelivered
  files"` WARN means rclone's own aggregate error tally and the job's
  per-file reconciliation diverged -- worth reporting, not necessarily a
  fault in either. Since ADR-0151 it says **"fully accounted for by
  case-colliding source keys"** when that is the whole explanation, and
  carries `case_colliding_keys` and `rclone_duplicate_notices` either way;
  the latter counts rclone's own `notice`-level `Duplicate object found in
  source` lines -- the signal ADR-0144 traced but nothing read.

  **Since ADR-0152 expect both to be quiet.** Case-colliding keys are now
  pulled individually, so the bulk pull sees no duplicate
  (`rclone_duplicate_notices` should be `0`) and a collision no longer
  produces an undelivered file. Look instead for
  `"transform: case-variant pull complete"` with `pulled`/`failed` counts,
  and for the `"source keys differ only by letter case or Unicode
  normalization…"` line (the normalization half since ADR-0158; before it
  read `"…only by letter case…"`), which is now
  **INFO** rather than WARN -- it describes what was done, not data about to
  be lost. A non-zero `failed` there, or a `Pull`-stage failure whose detail
  names a failed individual pull, is the real signal.

  **`soffice` failures since ADR-0158.** A transcode error saying
  `soffice crashed (signal N) … -- converter crashed` is the converter
  dying, not the file being unreadable: the local copy is kept, and macOS
  writes a matching `~/Library/Logs/DiagnosticReports/soffice-*.ips` with
  the faulting stack (parent process `pigeon`). On an older build the same
  crash reads `soffice produced no usable .pdf … (exit None) -- is not a
  readable document` -- `exit None` is the tell. A run where every failure
  shares one character class in its filename (a space, `#`, `%`) is
  ADR-0158's URL-encoding bug, not bad files.

  **Since ADR-0156** the variants are pulled by one bulk `copy` per
  collision-free *slot* (`case-variant pull complete` gains `slots`/`keys`;
  expect it seconds after `pull phase starting`, not minutes), and the
  collision notice is **one** line with `groups`/`keys`/`largest_group` and
  ten `examples` -- the full group list is at the end of
  `transform-report.txt`. Three more lines make a run readable while it is
  still going: `"transform: file failed"` (WARN, one per failed file, every
  kind and stage, with `relative_path`/`stage`/`error`; pull-stage ones cap at
  50 named plus one `not_named` aggregate), `"transform: progress"` (INFO,
  every 60 s, unconditional: `queued`, `in_flight`, `dispatched`,
  `completed`, `pushed`, per-stage failures, `pull_finished`,
  `pull_is_paused` -- identical consecutive lines mean a stall), and, for
  every job's streamed rclone leg, `"rclone: copy progress"` (INFO, at most
  every 60 s and only when cumulative `transferred`/`errors`/`bytes` moved --
  a gap is expected while ADR-0129 has the pull paused). Before ADR-0156 a
  non-`ffmpeg` kind's failures appeared **only** in the report.
- `deduplicate`: `download`, `archive` (zip expansion), `hash` (SHA-256
  content hashing), `place` (also `upload`). The fetch+hash phase is
  CPU-bound (ADR-0088) -- a slow `deduplicate` run with normal-looking
  `pigeon_resource_cpu_percent` (Prometheus, step 4) and a thin WARN/ERROR
  tail is more likely genuinely waiting on a slow source bucket than
  failing; a `deduplicate` run OOM-killed specifically partway through the
  upload phase (`pigeon_job_macro_phase` last reading `1` before samples
  stop, step 4) matches the known ADR-0089 failure mode -- check whether
  `--upload-only` (resumes uploading an already-completed local run
  without repeating download/hash/placement) is available on the
  installed version before suggesting a full rerun.

**`pigeon_upload_attempts_total` and `pigeon_upload_duration_seconds` no
longer exist** (ADR-0147). Both were emitted only from `upload.rs`, which
was deleted with the last two jobs that used it, so a run from a current
build has neither series -- don't read their absence as a fault.
`pigeon_upload_bytes_total` and `pigeon_upload_outcomes_total` **do** still
exist, emitted by `core::rclone` for every rclone-backed job. Which file
was in flight at crash time is not directly nameable from metrics (they
carry a `pigeon_job` label, not a per-file path); for the jobs that push
per-object, the last `phase="push"` increment is the closest signal.

**`pigeon_job_macro_phase` has no `pigeon_job="transform"` series** (ADR-0130).
`transform`'s pipeline pushes each file as soon as it is ready, so it has no
one-way local-work-then-upload transition for the gauge to describe, and it
deliberately stopped emitting it rather than report a transition that never
happens. Don't read its absence as a crash before upload -- use that job's
`pigeon_job_phase_total{phase="push"}` instead.

**Each job's printed completion summary** (not in the JSONL -- this is
what step 5 cross-checks the log's tally against; ask for it if the user
hasn't already pasted it):
- `email-import`: `"Delivered {delivered} message(s) and
  {attachments_pushed} attachment(s) ({objects_pushed} object(s) pushed),
  {failed} failed ({connect} connect, {examine} examine, {fetch} fetch,
  {missing_file} missing-file, {push} push)."` -- optionally followed by a
  line counting messages that could not be parsed (their raw `.eml` was
  still delivered) and one counting failures that happened after an
  endpoint stayed unreachable past the recovery budget.
- `transform`: `"Pulled {pulled} file(s); {transcoded} transcoded,
  {copied_through} copied through, {failed} failed; pushed {pushed}
  file(s)."` -- followed, from ADR-0130 on, by an optional
  `"Skipped {n} already done, {m} AppleDouble sidecar(s)."` (plus an Excel
  lock-stub count since ADR-0151) and, when the
  run failed, `"Failures by stage: {p} pull, {t} transcode, {pl} place,
  {pu} push."`. A non-zero `pull` stage count means files this run intended
  to download never arrived -- before ADR-0130 those were silently dropped
  and the run could report success, so a pre-ADR-0130 `transform` run's
  "success" does not rule out missing files.
- `deduplicate`: `"Processed {processed} file(s), {failed} failed ({download}
  download, {archive} archive, {hash} hash), {duplicates_skipped}
  duplicate(s) skipped, {dropped_members} zip member(s) dropped (extraction
  cap), {uploaded} uploaded, {unchanged} unchanged, {upload_failed} upload
  failed."` -- a `--upload-only` resumed run instead prints `"Uploaded
  {uploaded} file(s), {unchanged} unchanged, {upload_failed} upload
  failed."` (no `processed`/`failed`/dedup counts -- it never re-touches
  download/hash/placement, ADR-0089). `dropped_members` (ADR-0098) is
  already folded into the `archive` breakdown and `failed` -- it means real
  data was discarded by the per-archive extraction-ratio cap, not just
  noise; the affected root zip is excluded from `.processed`, so a plain
  rerun retries it.

Every job above: a nonzero `failed`/`upload_failed` means the process
exited with `FAILURE_EXIT_CODE`, not `0`.

## Known limitations

- **Two concurrent runs of the same command** are separable by `run_id`
  since ADR-0151 (step 2), **except** for each run's pre-wizard lines,
  which carry no id. On a pre-ADR-0151 log nothing in the schema separates
  them at all; narrow by timestamp as tightly as possible and say so
  explicitly in the takeaway rather than presenting a guess as certain.
- **A very fast crash may have 0-1 resource/progress metric samples** (the
  sampler runs every 5s, Alloy's own scrape interval is a separate,
  independently-configured cadence on top of that) -- don't expect a smooth
  trend on a run that failed in under a few seconds; lean on the WARN/ERROR
  events and any panic entry instead.
- **Resource/progress data (step 4) only exists if Cockpit/Alloy was
  actually deployed and running on that machine** (ADR-0093) -- unlike
  before, the JSONL log is no longer self-sufficient for this. On a machine
  without it, a crash has zero resource telemetry anywhere, and the
  takeaway should say so plainly rather than reasoning from absence.
- This is a documented procedure, not a tool -- there is no `pigeon logs`
  subcommand. Everything here is plain `jq` against the raw file (plus,
  since ADR-0093, a Prometheus/Cockpit query for step 4's data when it's
  available).
