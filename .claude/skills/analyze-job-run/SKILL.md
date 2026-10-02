---
name: analyze-job-run
description: Use this skill when diagnosing a `pigeon` job run from its logs -- reading the ADR-0073 JSONL observability log (`pigeon.jsonl`) to reconstruct a run's timeline, spot resource/failure patterns, and produce a written takeaway (root cause, evidence, confidence, next action). Trigger on requests like "can you check the logs", "what happened in the last run", "read the log dump and tell me what went wrong", "why did this job crash", or a pasted terminal transcript showing a job failure/crash/unexpected exit code.
---

# Analyzing a `pigeon` job run's logs

The ADR-0073 JSONL log is the only durable record of what happened during a
run -- when a run is killed (SIGKILL, OOM) nothing else gets a chance to log
gracefully on the way out, so the log is often the *only* evidence available.
This skill formalizes the ad hoc diagnostic process that first found the
ADR-0076 SIGKILL root cause (a real `pull-transform` crash against 50-100GB
zip archives), so it's repeatable instead of re-derived from source every
time. Follow it in full; don't skip straight to guessing a cause.

## Steps

1. **Locate the log.** Default path:
   `~/Library/Application Support/pigeon/logs/pigeon.jsonl` (macOS) or
   `~/.local/share/pigeon/logs/pigeon.jsonl` (Linux -- `directories`'
   `data_local_dir()`, i.e. `$XDG_DATA_HOME` or `~/.local/share` by
   default; this is the path a headless Linux box running a long `job run
   dedupe`/`pull-transform` will actually have, ADR-0089). Overridable two
   ways -- check both before assuming the default: the `PIGEON_LOG_DIR`
   environment variable, or a `--log-file <path>` flag passed to the
   command itself. If the user pasted a terminal transcript, look for
   either of these before reading anything.

   **If the symptom is a SIGKILL/crash with no graceful exit, check for an
   OOM kill before reading the JSONL at all**: `dmesg | grep -i oom` or
   `journalctl -k | grep -i oom` (Linux). The kernel's own OOM-killer log
   line (timestamp + `anon-rss:<kB>`) is faster to get to than reconstructing
   the same conclusion from `resource_sample` climbing, and dates/correlates
   directly against the JSONL timeline from step 3 once you have it
   (ADR-0089 -- this is exactly how a real `dedupe` OOM mid-upload was
   first confirmed).

2. **Isolate the run.** The log is **append-only across every past
   invocation** -- one file accumulates lines from every run of every
   command, interleaved in chronological order, forever (no rotation, per
   ADR-0073). There is **no run-ID or PID field anywhere in the schema** --
   confirmed by grepping the whole crate for `process::id`/`run_id`/`Uuid`,
   zero hits. The only two signals available to isolate "the run I care
   about":
   - The outer span's `command` field identifies *which command*, not
     *which invocation* -- filter to it first:
     ```
     jq 'select(.spans[]?.command == "job.pull-transform")' pigeon.jsonl
     ```
     (swap in `job.email-sync`, `job.decrypt-files`, `keyring.add`,
     `keyring.modify`, `keyring.delete`, or `keyring.list` as needed --
     these are the exact `command_name()` strings, see the cheat sheet
     below.)
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

4. **Pull the `resource_sample` stream** (CPU/memory/disk-I/O, sampled
   every 5 seconds while a job runs):
   ```
   jq 'select(.fields.kind == "resource_sample")' pigeon.jsonl
   ```
   Fields: `cpu_percent`, `mem_bytes`, `disk_read_bytes`,
   `disk_written_bytes` (all `fields.*`, cumulative process counters from
   `sysinfo`, not per-interval deltas -- diff consecutive samples for a
   rate). Read for:
   - **`mem_bytes` climbing without bound** across samples -> memory
     pressure/a leak/an unbounded in-memory buffer.
   - **`disk_read_bytes` far exceeding any known input size** for the
     run (e.g. gigabytes of "read" against a bucket whose objects are
     only megabytes) -> the OS-level swap-thrashing signature that
     confirmed ADR-0076's root cause -- the process wasn't really reading
     that much real data, it was being paged in and out under memory
     pressure.
   - **Samples stopping abruptly** well before any expected completion
     -> the process died at roughly that timestamp (cross-check against
     step 2's "no closing event" finding).

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

Reflects the schema as of ADR-0073/0074/0075/0076/0077/0089 (2026-10-02). If a
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
  `pigeon::commands::job::pull_transform::worker`).
- `span` -- the *innermost* currently-active span (name + its own fields).
- `spans` -- the full ancestor span stack, outermost to innermost, same
  shape as `span` per entry. Note: on a normal event, `spans` includes the
  span itself; on the auto-emitted `"close"` event for a span, `spans` is
  empty while `span` still names the closing span.

**`command_name()` values** (the `command` field on the outermost
`"command"` span -- this is what step 2's filter matches on):
`job.email-sync`, `job.pull-transform`, `job.decrypt-files`, `job.dedupe`,
`job.sort`, `job.email-pull`, `keyring.add`, `keyring.modify`,
`keyring.delete`, `keyring.list`.

**Keyring commands emit no per-operation tracing at all** -- only the two
boilerplate `"command finished"`/`"close"` events for the outer span. A
keyring-specific issue (a bad alias, a keychain prompt, a validation
failure) is **not diagnosable from this log** -- it only ever went to
stderr via `eprintln!`. Don't spend time searching the JSONL for it; ask
the user for the terminal output instead.

**Per-job `step` vocabulary** (WARN/ERROR events' `fields.step`):
- `pull-transform`: `download`, `archive`, `classify`, `placement`
  (also `upload`, shared with every job's upload phase).
- `email-sync`: `connect`, `examine`, `batch`, `fetch`, `transform`,
  `verify` (also `upload`).
- `dedupe`: `download`, `archive` (zip expansion), `hash` (SHA-256 content
  hashing), `place` (also `upload`). The fetch+hash phase is CPU-bound
  (ADR-0088) -- a slow `dedupe` run with normal-looking `resource_sample`
  CPU and a thin WARN/ERROR tail is more likely genuinely waiting on a slow
  source bucket than failing; a `dedupe` run OOM-killed specifically
  partway through the upload phase (last JSONL lines are `step = "upload"`
  or a run of `"upload started"` events with no matching `"command
  finished"`) matches the known ADR-0089 failure mode -- check whether
  `--upload-only` (resumes uploading an already-completed local run without
  repeating download/hash/placement) is available on the installed version
  before suggesting a full rerun.
- `sort`: `download`, `place` (also `upload`).

**`"upload started"` event** (`upload.rs::upload_one`, ADR-0089, INFO
level, every job's upload phase): `fields.file` (the local path) and
`fields.bytes` (its size), logged right before the upload attempt begins --
since there's no matching `"command finished"`/per-file completion event on
a crash, the *last* `"upload started"` line(s) before the log goes silent
name whichever file(s) were actually in flight when the process died
(`jq 'select(.fields.message == "upload started")' pigeon.jsonl | tail`).

**Each job's printed completion summary** (not in the JSONL -- this is
what step 5 cross-checks the log's tally against; ask for it if the user
hasn't already pasted it):
- `email-sync`: `"Synced {synced} message(s), {failed} failed ({connect}
  connect, {examine} examine, {batch_error} batch-error, {verification}
  verification, {parse_skipped} parse-skipped, {missing_file}
  missing-file), {merged_messages} message(s) merged, {deduped_attachments}
  attachment(s) deduped, {uploaded} uploaded, {unchanged} unchanged,
  {upload_failed} upload failed."` followed by an attachments-estimate line.
- `pull-transform`: `"Processed {processed} file(s), {failed} failed
  ({download} download, {archive} archive, {classify} classify,
  {placement} placement), {skipped_type} skipped (type not selected),
  {duplicates_skipped} duplicate(s) skipped, {recoded} recoded,
  {recode_fallback_to_original} kept as original (recode did not verify),
  {uploaded} uploaded, {unchanged} unchanged, {upload_failed} upload
  failed."`
- `decrypt-files`: `"Decrypted {decrypted} file(s), {failed} failed."`
- `dedupe`: `"Processed {processed} file(s), {failed} failed ({download}
  download, {archive} archive, {hash} hash), {duplicates_skipped}
  duplicate(s) skipped, {uploaded} uploaded, {unchanged} unchanged,
  {upload_failed} upload failed."` -- a `--upload-only` resumed run instead
  prints `"Uploaded {uploaded} file(s), {unchanged} unchanged,
  {upload_failed} upload failed."` (no `processed`/`failed`/dedup counts --
  it never re-touches download/hash/placement, ADR-0089).
- `sort`: `"Placed {placed} file(s), {failed} failed ({download} download,
  {placement} placement), {uploaded} uploaded, {unchanged} unchanged,
  {upload_failed} upload failed."`

Every job above: a nonzero `failed`/`upload_failed` means the process
exited with `FAILURE_EXIT_CODE`, not `0`.

## Known limitations

- **No way to disambiguate two concurrent runs of the same command** --
  nothing in the schema separates them; if this matters, narrow by
  timestamp as tightly as possible and say so explicitly in the takeaway
  rather than presenting a guess as certain.
- **A very fast crash may have 0-1 `resource_sample` points** (5-second
  sampling interval) -- don't expect a smooth trend on a run that failed
  in under a few seconds; lean on the WARN/ERROR events and any panic
  entry instead.
- This is a documented procedure, not a tool -- there is no `pigeon logs`
  subcommand. Everything here is plain `jq` against the raw file.
