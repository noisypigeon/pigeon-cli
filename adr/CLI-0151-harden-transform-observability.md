# CLI-0151: what the first real xlsx run surfaced

- **Author**: Willow Graysen ([@noisypigeon](https://github.com/noisypigeon)).
- **Date**: 2026-10-14.
- **Status**: Accepted.

## Context

The first real `job run transform --input-file-type xlsx` run
(2026-10-14T04:08:00Z, local output `/tmp/pigeon-transform-xlsx-job`)
**largely succeeded**: 266 of 267 intended files converted into **1318
CSVs**, with 0 transcode, 0 place, and 0 push failures, every output name
unique, all counts reconciling, and `source/`/`result/`/`scratch/` all left
empty -- so CLI-0124's GC and CLI-0149's new RAII scratch guard both did
their job. CLI-0149's naming behaved exactly as designed: single-sheet
workbooks took the plain name, and a 27-sheet workbook produced
`01of27`…`27of27`.

Its one failure was already-known and correctly handled. Two source keys
differing only by case (`Bookkeeping.xlsx` / `bookkeeping.xlsx`) meant
rclone delivered one and ignored the other -- detected up front by
CLI-0144, attributed per-file with the right explanation, exit 1. Issue
[#83](https://github.com/noisypigeon/pigeon-cli/issues/83) tracks pulling
both variants.

Reading that run's logs and artifacts end to end surfaced six things worth
fixing. Two are CLI-0149's, three predate it, and one is a polish.

## Decision

### 1. A strict stem, for spreadsheet kinds only

The run produced a destination name of

```text
Budget (Normal, Reduced, Crisis)-ee7a9c8a4c2e5342.csv
```

That is **one** filename containing two commas, sitting in
`transform-report.txt`'s **comma-joined** `destination_filenames` column.
Any consumer splitting that column reads three phantom files. 5 of 267
source names in this run contain a comma.

CLI-0149's own amendment asserted the join was safe "only because [the
slug's] set excludes `,`". That covered the *slug* half of the name and
missed the *stem*, which comes from `sanitize_filename` -- a function that
does **no character filtering whatever**, only final-path-component
extraction and truncation. So commas, tabs, and newlines pass straight
through from the S3 key. The fix was half a fix.

A new `core::data::strict_stem` applies `sheet_slug`'s character set at
`MAX_FILENAME_LENGTH`'s larger budget (they share a charset, not a length),
and `placement::group_key` uses it **only when the input kind is a
spreadsheet**.

**Why scoped, and why that is sufficient rather than timid.** The
separator can only mislead when the column holds *more than one* filename,
and only spreadsheet kinds produce that -- a jpg run's column holds exactly
one value, where an embedded comma is unambiguous. So scoping closes the
bug completely.

**Why scoped, and why that is necessary rather than fastidious.**
`group_key` feeds `compute_destination_name` for *every* input kind, and
`transform` never deletes at the destination (CLI-0112). An unscoped change
would therefore not rename already-written objects -- it would
**duplicate** them, leaving the originals behind. Measured against this
run: 67% of inputs (179/266) would rename, 1001 of 1318 objects re-push. At
that rate CLI-0144's 37,955-object jpg bucket would gain roughly 25,000
duplicate objects on its next run, and the same again for mp4 and opus.
Confined to spreadsheets, the cost is re-pushing 1001 CSVs (~22 MB) once.

The hash input is untouched, so the digest is unchanged -- verified against
this run's own `ee7a9c8a4c2e5342`. Mechanically, `group_key`,
`compute_destination_name`, `sheet_destination_name`, and `place_one` now
take the `InputFileType` rather than a bare `output_extension` string,
which also removes the two `let _ = output_extension;` warts CLI-0149 left
behind in `enqueue` and `files_needing_pull`.

### 2. The lock-file skip gets its own counter

CLI-0149's `~$` Excel-lock filter was counted two different wrong ways:

- `files_needing_pull` incremented **`skipped_apple_double`**, so the log
  and report named it an AppleDouble sidecar. This run reported "2
  AppleDouble sidecar(s)" and **no artifact can say** whether those were
  `._*` stubs, `~$*` stubs, or one of each.
- `enqueue` returned `EnqueueOutcome::SkippedLockFile`, but all three call
  sites compared `== EnqueueOutcome::SkippedAppleDouble` -- so a lock file
  reaching *that* choke point (the on-disk scan, or rclone's live tail) was
  **counted nowhere at all**.

`PullSelection` and `TransformSummary` gain `skipped_lock_file`, the three
`enqueue` call sites `match` on both variants, and the report and
`"transform: run complete"` line name each separately. The `EnqueueOutcome`
doc comment -- still claiming "Only `SkippedAppleDouble` is counted by the
caller" -- is corrected.

### 3. Backpressure checks whether the pull is still running

```text
04:08:14.595  rclone: copy complete (transferred 266, errors 0)
04:08:14.962  WARN transform: failed to pause pull process  pid=24759  "kill SIGSTOP failed"
04:13:43.677  WARN transform: failed to resume pull process
```

CLI-0129's backpressure block had no `pull_finished` guard. 367 ms after
the pull exited it signalled the dead pid, then believed the pull was paused
for the next **5 m 29 s** and tried to resume it. Two misleading WARNs,
neither of which a dead process could act on.

The condition becomes `if let (Some(pid), false) = (pull_pid,
pull_finished)`, and `pull_is_paused` resets when the pull completes -- an
exited process is not paused. Reliably triggered by small inputs: here the
pull took 11 s while local work took 5 m 35 s, so the backlog was still
above the high-water mark when the pull drained.

### 4. `run_id` in the log, a per-run artifact, and rotation

`pigeon.jsonl` had reached **326 MB / 696,608 lines**, spanning three
weeks, with no rotation (CLI-0073) -- and CLI-0100 uploads it **whole** as
every run's artifact. This run's upload **did not complete**: its rclone log
ends `Signal received: interrupt / Exiting...`, so the run lost its log
artifact entirely, while the report and transcript (87 KB, 199 B) uploaded
in under a second each.

Three changes, in dependency order:

- **`run_id` on the `command` span.** Declared `tracing::field::Empty` at
  span creation and recorded by `report_upload::generate_run_id` -- the one
  chokepoint every job already funnels through, so none of the eight
  wizards changes. The log is filterable to a single invocation for the
  first time, closing the gap the `analyze-job-run` runbook documents as a
  known limitation. Stated cost: the handful of lines emitted before the
  wizard runs (notably `"command started"`) cannot carry it, because the id
  does not exist yet.
- **The artifact is this run's lines.** `write_run_log_extract` greps every
  generation for `"run_id":"<id>"` and uploads just those, matched by
  substring rather than by parsing 326 MB of JSON to answer a question
  `contains` already answers. This reverses CLI-0100's explicit
  "uploaded whole, not filtered to this run", which was a reasonable choice
  at a reasonable file size and had become one that costs a run its
  artifact.
- **Daily rotation, 14 generations**, via `tracing_appender`'s builder
  (already a dependency). The active file gains a date
  (`pigeon.2026-10-14.jsonl`), so the extract globs the directory rather
  than naming one file -- which also means a run straddling midnight is
  reassembled from both generations rather than half-lost.

**`--log-file` is deliberately exempt.** That flag names a *file*, and
rotating a path the operator chose would silently write somewhere else, so
only the default location rotates. A pre-rotation `pigeon.jsonl` is left
exactly where it is: it is the operator's history to keep or delete, not
this code's to remove.

### 5. A push log is deleted with its output

`.staging/push-logs/` held **1318 files / 5.1 MB** after the run, one per
*output* object, and nothing has ever removed them. CLI-0124 GCs `source/`
and `result/` but not these; CLI-0149 multiplied the count ~5× by pushing
per output rather than per input, and CLI-0144's jpg run would have left
37,955.

Each log is now removed alongside its output once that push succeeds, in
the same best-effort spirit as `remove_local_copy`. A **failed** push keeps
its log -- that is the one case where it has diagnostic value, matching
CLI-0124's retain-on-failure rule, and the cleanup line is only reached
after success.

### 6. The cross-check explains itself

`transform: rclone's pull error count disagrees with the number of
undelivered files` fired with `rclone_errors=0, undelivered_files=1`. The
job had *already* detected the collision up front, and rclone's own log
carries exactly one `notice`-level `Duplicate object found in source -
ignoring` -- a level nothing in this crate read, which is precisely why
CLI-0130's cross-check fires on a run where nothing is wrong. CLI-0144
traced this mechanism but only out of rclone's *stderr*.

`RcloneLogSummary` gains `duplicate_notices`, and when the undelivered
count is fully accounted for by detected case collisions the warning says
so. Both sides of the comparison are now stated rather than inferred.

## Consequences

- **Spreadsheet destination names change**, and 1001 of this run's 1318
  CSVs will be re-pushed on the next run under their new names. The old
  objects remain, because this job never deletes -- a one-time duplication
  bounded to the spreadsheet prefix, and the explicit price of §1.
- **Image, video, and audio destination names are byte-identical**, pinned
  by a test. That is the property §1's scoping exists to protect, and
  without it every already-processed bucket would have duplicated.
- A run's log artifact goes from 326 MB to kilobytes, and actually
  completes. Diagnosing a run no longer means isolating it by timestamp
  from a shared file.
- `pigeon.jsonl` is no longer a single unbounded file. **An existing one is
  not migrated or deleted**; the new rotated files appear beside it.
- The runbook's documented log path becomes `pigeon.<date>.jsonl` at the
  default location, and `.claude/skills/analyze-job-run` is updated in this
  same PR, per its own rule.
- Two WARNs per run disappear, and the skip counts become answerable.

## Out of scope

- **Pulling both members of a case-colliding key pair** -- unchanged, still
  [#83](https://github.com/noisypigeon/pigeon-cli/issues/83). §6 explains
  the disagreement; it does not resolve the data gap.
- **Migrating or deleting the pre-rotation `pigeon.jsonl`**, or the
  spreadsheet objects §1 orphans. Both are the operator's call, and this
  job has never deleted anything.
- **A strict stem for the media kinds** -- deliberately rejected in §1,
  with the duplication arithmetic stated.
- **Quoting `source_relative_path` and `detail` in the report.** A source
  key containing a tab would still break that row's format. §1 closes the
  multi-value column, which is the one CLI-0149 opened; the single-value
  columns have the same pre-existing exposure and are a separate decision.
- **`sanitize_filename`'s leniency in general** -- it is load-bearing for
  every other job and for every already-written object.

## Amends

CLI-0073 (no rotation; no `run_id` in the schema), CLI-0100 (whole-log
artifact upload), CLI-0116 (the per-file push log now has a lifetime),
CLI-0124 (its GC extends to push logs), CLI-0129 (backpressure gains the
`pull_finished` guard its signalling always needed), and CLI-0149 (its
amendment's claim about the comma-joined column was half-right, and its
lock-file counting was wrong two ways). Each gets a short
forward-pointing "Amendment" section.

## Verification

- `mise run ci` clean (fmt-check + lint + test).
- `core/data.rs`: `strict_stem` removes what `sanitize_filename` lets
  through -- asserted against the real `Budget (Normal, Reduced, Crisis)`
  name, including that `sanitize_filename` genuinely keeps its commas --
  and uses the filename budget rather than the slug's.
- `placement.rs`: a spreadsheet name carries no `,`/tab/newline/space/paren;
  **a png/jpeg/mov/mp4-audio name still starts with the raw stem verbatim**;
  the digest does not depend on how the stem renders; a strict-stem group is
  still recognised by `DestinationGroups`.
- `worker.rs`: `enqueue` returns `SkippedLockFile` for `~$` and
  `SkippedAppleDouble` for `._` (the former counted nowhere before);
  `files_needing_pull` tallies them separately; the multi-sheet end-to-end
  test asserts `push-logs/` is empty afterward, and the failure test asserts
  a file that never pushed leaves no log.
- `core/rclone/log.rs`: a `notice` duplicate-object line increments
  `duplicate_notices` while leaving `errors` at 0 -- the asymmetry the whole
  of §6 rests on; an unrelated `notice` does not.
- `report_upload.rs`: the extract keeps only lines carrying this run's id,
  spans rotated generations, is empty when nothing matches, and tolerates a
  generation vanishing mid-read.
- `tests/cli.rs`: `--log-file` still writes exactly the named file
  (`log_file_flag_writes_valid_jsonl_with_the_command_name`, unmodified) --
  the regression check on §4's rotation exemption.
- Manual: re-run against the same source and confirm zero
  `failed to pause/resume` WARNs, skip counts split by kind, an empty
  `push-logs/`, a kilobyte-scale log artifact that completes, and that a
  second re-run is a clean no-op -- which is what proves
  `DestinationGroups` recognises the renamed spreadsheet outputs.

## Amendment (2026-10-14): §6's explanation becomes a fallback (CLI-0152)

§6 made the `rclone_errors` vs. `undelivered_files` cross-check say "fully
accounted for by case-colliding source keys" when that was the whole
explanation. It was, on the run this ADR analysed, and the wording was
correct. CLI-0152 then removes the cause: every colliding key is pulled
individually, so a collision no longer produces an undelivered file and the
bulk pull no longer even sees a duplicate -- `duplicate_notices` should read
**0**.

Both the message and `duplicate_notices` stay, as a fallback for a colliding
key whose own pull fails. Expect that path to be quiet; a run where it fires
is reporting a real per-key pull failure, not the ordinary collision this ADR
was describing.

Everything else in this ADR is unaffected. Its six fixes were all confirmed
working by the rerun that prompted CLI-0152: lock stubs counted separately
(revealing that run's "2 AppleDouble" were in fact both `~$` stubs), zero
spurious pause/resume WARNs, the log rotated and isolable by `run_id`, the log
artifact down from 326 MB-interrupted to 3.198 KiB-complete, and
`push-logs/` empty.

## Amendment (2026-10-14): §1's scope is deliberately preserved (CLI-0154)

CLI-0154 adds thirteen document input kinds and does **not** extend
`strict_stem` to any of them, for the reason §1 gives: the strict charset earns
its place only where the report column holds more than one value, and only
multi-output kinds produce that. Every document kind is single-output.

The measurement in §1 is what makes this a decision rather than an omission --
widening `group_key`'s charset does not rename already-written objects, it
duplicates them, at the 67%-of-inputs rate measured here. CLI-0154 pins the
property with a test asserting a document destination name is byte-identical to
today's media naming.

## Amendment (2026-10-15): collision notices no longer regrow the log (CLI-0156)

CLI-0152's one-INFO-per-colliding-group notice put back the per-run log volume
this ADR cut (428 of one run's 430 lines). CLI-0156 collapses it to one
aggregate line and moves the full list into `transform-report.txt`, and adds
per-file failure WARNs plus 60 s progress lines, both bounded.
