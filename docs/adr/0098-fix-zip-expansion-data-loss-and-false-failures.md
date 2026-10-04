# ADR-0098: fix silent zip-expansion data loss and false archive failures

- **Author**: Willow Finch ([@noisypigeon](https://github.com/noisypigeon)).
- **Date**: 2026-10-04.
- **Status**: Accepted.

## Context

Two separate `job run deduplicate` runs were log-analyzed end to end.

**Run 1** (01:28-05:23 UTC): exited 1 after 3h55m with 21 archive failures,
every one a key matching `…zip!__MACOSX/…/._<name>.zip` -- macOS
AppleDouble resource-fork stubs, not real data. Every dedup/placement/
upload count reconciled exactly; the run only "failed" because these 21
non-issues were counted the same as a genuine failure.

**Run 2** (01:56-05:20 UTC, a larger/different bucket): exited 1 after
3h25m with 50 archive failures of the identical AppleDouble pattern --
but also, critically, **68,376 "dropped zip member during extraction"
warnings**, every one `"extraction cap (536870912000 bytes) exceeded"`.
This source held roughly 526 GB of zips (Takeout/Facebook/Instagram/
Discord exports, mostly incompressible media), so the run legitimately
exceeded the 500 GiB cap partway through -- after which **every non-empty
member of every zip processed for the rest of the run was silently
dropped**: on the order of 30k jpg, 5.5k png, 2.5k mp4, 1.8k heic, 1.5k
mov, 1k pdf, and more. None of this shows up in `failed`, in the exit
code, or in any summary field. Worse, **a plain rerun cannot recover it**:
a root zip is checkpointed into `.processed` as soon as it's expanded,
regardless of what happened to its members, so the already-poisoned roots
are permanently skipped on any future run against this bucket.

This is the deferred item from ADR-0091's "Out of scope" section ("the 53
unopened nested zips ... silently dropped without being duplicates of
anything ... being decided and written up separately") -- except the real
mechanism turned out to be broader than "unopenable" zips specifically:
any zip, openable or not, can lose members once the run-wide extraction
cap is hit.

Every claim below was verified directly against `main` at commit `aad1b91`
(the latest merged work, ADR-0097), since both runs predated it:

1. **AppleDouble/`__MACOSX` entries aren't filtered anywhere.**
   `expand_to_dir` (`src/commands/job/pull_transform/archive.rs:57-102`,
   shared by `pull-transform` and `deduplicate`) only skips
   `entry.is_dir()`; `is_zip_key` (`src/commands/job/deduplicate/worker.rs:
   38-40`) is purely extension-based. A `._foo.zip` AppleDouble stub gets
   extracted as a normal member, then requeued as a nested zip (it ends in
   `.zip`), then fails `ZipArchive::new` ("Could not find EOCD") and counts
   as `FailureCategory::Archive` -> `FailureBreakdown.archive` ->
   `DeduplicateSummary.failed` (`worker.rs:465-470`) -> `FAILURE_EXIT_CODE`
   (`wizard.rs:249`; `pull_transform/wizard.rs:563` follows the identical
   pattern).
2. **The extraction cap is one process-wide cumulative counter, and
   exceeding it silently drops the member instead of failing it.**
   `copy_capped` (`archive.rs:111-136`) takes `total_extracted: &AtomicU64`
   shared across the *entire run*, not per archive, and on overflow returns
   `Err`, which `expand_to_dir` (lines 90-98) catches and only
   `tracing::warn!`s -- the member is never counted anywhere, never
   returned to the caller. `MAX_TOTAL_EXTRACTED_BYTES` is 500 GiB
   (`archive.rs:31`), trivially exceedable by one large real-world bucket.
   The function's own doc comment (lines 51-56) already claims a dropped
   member is "tallied as a failure for that one member" -- the code
   doesn't match its own doc comment.
3. **A root zip is checkpointed unconditionally once expanded, independent
   of whether its members survived.** `ItemOutcome::ZipExpanded` pushes
   `display_key` into `finished_root_keys` the moment expansion returns
   `Ok` (`worker.rs:363-365`), with no link to whether any member later
   failed or was dropped. At the end of the run,
   `finished_root_keys.retain(|key| placed_keys.contains(key) ||
   is_zip_key(key))` (`worker.rs:458`) unconditionally keeps every
   zip-rooted key regardless of `placed_keys`, and every surviving key is
   written to `.processed` via `append_checkpoint` (lines 459-461). A root
   zip with 1,000 dropped members checkpoints exactly the same as one with
   zero -- a later rerun skips every already-`.processed` root zip and
   never retries its dropped members.
4. **Archive-phase warnings don't carry the `command` tracing span**,
   breaking the `analyze-job-run` skill's own documented filter (`jq
   'select(.spans[]?.command == "job.deduplicate")'`,
   `.claude/skills/analyze-job-run/SKILL.md:63,179`). `run_instrumented`
   (`src/observability/mod.rs:120-127`) enters the `"command"` span via a
   manual `span.enter()` guard on the calling thread -- sound for
   synchronous code, but every `tracing::warn!`/`error!` inside
   `process_item`'s `tokio::spawn`ed worker tasks and
   `tokio::task::spawn_blocking` closures (`worker.rs:178-186,226,326`)
   runs without that span attached, since neither `tokio::spawn` nor
   `spawn_blocking` inherit the calling thread's span context
   automatically. No `.instrument(Span::current())` exists anywhere in
   this path.
5. **The drop warning doesn't name the containing archive**, only the
   member (`archive.rs:92-97`, `member = %name`) -- so even with span
   propagation fixed, there's no way to tell which root zips are incomplete
   from the log alone without also fixing (3).
6. **The upload span still mislabels its bucket.** `upload_one`'s
   `#[tracing::instrument(fields(identity = %task.label, ...))]`
   (`src/commands/job/upload.rs:218-221`) and its matching
   `tracing::warn!` (line 317) carry `task.label`, and every job's normal
   (non-`--upload-only`) call into `upload_result` --
   `deduplicate/worker.rs:485`, `pull_transform/worker.rs:923`,
   `reduce/worker.rs:232` -- still passes the **source** bucket's alias,
   even though the upload always targets `remote` (the destination).
   ADR-0097 fixed the equivalent *metric* label (`destination_bucket`) but
   never touched this span field.
7. **Empty-file collapsing under content-hash dedup is by design, not a
   bug.** `ContentIndex` (`src/core/data.rs:36-90`) has no size awareness
   at all, and ADR-0082 explicitly documents hash equality as deliberately
   "byte-identical, not semantic." Noted here so it isn't silently
   re-raised; no fix proposed.

Item 3 is the one that matters most: silent, unbounded (scales with bucket
size against a fixed 500 GiB cap), and -- unlike every other finding here,
and unlike this codebase's usual checkpoint-is-safe-to-rerun posture (ADR-
0030, ADR-0034) -- **not self-healing on rerun**.

## Decision

### 1. Replace the global extraction cap with a per-archive compression-ratio guard

`copy_capped`/`expand_to_dir` stop sharing one process-wide `AtomicU64`
budget. Each call to `expand_to_dir` computes its own archive's total
compressed size (summed from the `ZipArchive`'s entries before extraction
starts) and caps extraction at `ratio × compressed_size` (100x) for that
archive only -- still a real zip-bomb guard (a legitimate archive
practically never approaches 100:1 on already-compressed media), but no
longer penalized by how much *other*, unrelated, legitimately large data
the rest of the run has already extracted. `download::check_disk_space`
(already called before each expansion, `worker.rs:167`) remains the
real-time disk backstop per ADR-0076, unchanged. `MAX_TOTAL_EXTRACTED_BYTES`
is removed; `MAX_ZIP_DEPTH` is untouched (nesting depth, not byte volume).

### 2. A dropped/capped member is counted, not just logged

`expand_to_dir` returns a dropped-member tally alongside its
`Vec<ExtractedMember>`. `deduplicate/worker.rs` folds this into
`FailureBreakdown.archive` -- making the code match its own doc comment --
and surfaces it in `DeduplicateSummary` distinctly enough that it's a real,
nonzero, exit-code-affecting signal: dropped bytes mean real user data was
discarded, unlike the AppleDouble case this ADR also fixes.
`pull_transform/worker.rs` (the other `expand_to_dir` consumer) gets the
same wiring.

### 3. Taint the root, don't checkpoint it, on any descendant failure

`QueueItem` gains a `root_key: String`, set to the item's own `display_key`
at depth 0 and propagated unchanged to every expanded descendant
(`worker.rs:192-201`). A new shared set (parallel to
`finished_root_keys`/`failure_breakdown`) records a root as tainted
whenever any descendant produces `ItemOutcome::Failed` or a dropped-member
event. At the end of `run_deduplicate_job`, the existing
`finished_root_keys.retain(...)` (`worker.rs:458`) additionally excludes
anything tainted before `append_checkpoint` runs, so a tainted root is
never written to `.processed` and a subsequent run retries it. The
existing `.content-hashes` index means every member that *did* succeed the
first time becomes a cheap duplicate on the retry, not re-work.
`pull_transform/worker.rs` gets the equivalent treatment.

### 4. Skip AppleDouble/`__MACOSX` entries instead of treating them as zips

In `expand_to_dir`, skip (no disk write, no returned member) any entry
whose name starts with `__MACOSX/` or whose basename starts with `._` --
logged via `tracing::debug!`, not `warn!`, since this is expected noise,
not a failure. This removes the false archive failures in both analyzed
runs at the source, rather than reclassifying them after the fact.

### 5. Carry the `command` span through spawned/blocking work; name the archive in drop warnings

Every `tokio::spawn`/`tokio::task::spawn_blocking` call in `process_item`'s
path captures `tracing::Span::current()` before spawning and re-enters it
inside the spawned closure (`.instrument(span)` for the `tokio::spawn`ed
async task; `span.in_scope(|| ...)` inside each `spawn_blocking` closure,
since that's synchronous). Every archive/download/hash warning now carries
the `command`/`instance` fields the rest of the log already relies on,
fixing the `analyze-job-run` skill's documented
`spans[]?.command == "job.deduplicate"` filter. The drop warning also
gains the containing archive's own display key as a plain argument (not a
span field, since `expand_to_dir` itself has no span context), so "which
root zips are incomplete" is answerable from the log alone even before
cross-referencing fix 3. `pull_transform`'s equivalent spawn sites get the
same treatment, and `.claude/skills/analyze-job-run/SKILL.md` gets a
one-line update so its cheat sheet doesn't contradict actual behavior.

### 6. Fix the upload span's bucket label

Renames `upload.rs`'s `identity` tracing field to something that doesn't
imply "email identity" (`UploadTask.label`'s own doc comment already
describes it as "an identity alias, a source bucket alias, ..."), and --
the actual bug -- fixes every non-`--upload-only` `upload_result` call
site (`deduplicate/worker.rs:485`, `pull_transform/worker.rs:923`,
`reduce/worker.rs:232`) to pass the **destination** bucket's alias,
matching what `--upload-only` and the ADR-0097 metric labels already do
correctly. Removes the stale doc comment at `deduplicate/worker.rs:498-504`
that currently documents the wrong-bucket behavior as intentional.

### 7. Empty-file collapsing: explicitly not changed

No code change. Documented here as a deliberate non-fix, per ADR-0082's
existing byte-identical-not-semantic design decision.

## Consequences

- A `deduplicate`/`pull-transform` run no longer fails (or silently drops
  data) just because its source contains more legitimately large zip data
  than one prior run happened to -- the cap is now proportional to each
  archive's own size, not the whole job's cumulative total.
- Dropped members are a real, visible, exit-code-affecting failure
  category from now on, instead of invisible. A run that drops data will
  now correctly report `failed > 0` and exit nonzero.
- A root zip with any failed/dropped descendant is no longer checkpointed,
  so a rerun actually retries it -- restoring this job's usual
  checkpoint-is-safe-to-rerun guarantee for the one case that was silently
  violating it.
- macOS AppleDouble/`__MACOSX` noise no longer produces false archive
  failures, and -- as a side effect of being filtered before extraction --
  no longer lands in `result/`/the destination bucket as "unique" content
  either.
- Archive/download/hash warnings in `pigeon.jsonl` now carry the same
  `command`/`instance` span fields every other log line already has,
  closing a real gap in the `analyze-job-run` skill's documented filters.
- Upload-phase log lines for a normal (non-`--upload-only`) run now
  correctly identify the destination bucket instead of the source.

### Remediating the two already-analyzed buckets

Both buckets' `.staging/.processed` files were written under the old
(buggy) logic, and the new code has no way to distinguish
"legitimately complete" from "checkpointed-but-actually-tainted" after the
fact. Once this fix is merged: remove every zip-rooted line (`grep
'\.zip$'`) from each bucket's local `.staging/.processed`, then rerun `job
run deduplicate` against the same local output and destination.
`.content-hashes`/`.uploaded` turn already-placed/uploaded content into
cheap duplicates/unchanged uploads, so only genuinely missing members (
previously dropped, or never reached) get placed and uploaded. This
re-downloads the affected zips (roughly 526 GB for the larger bucket) --
confirm free disk space first. This is a manual, one-time step, not part
of the automated fix.

## Out of scope

- Password-protected or genuinely corrupt zips that fail to open at all
  remain a counted `archive` failure (unchanged from today) -- this ADR
  only changes what happens to members *inside* an openable zip once the
  (now per-archive) extraction budget is exceeded, plus the AppleDouble
  false-positive case. Whether an unopenable zip should instead be kept as
  an opaque blob is a separate behavior question, not addressed here.
- `pull_transform`'s own classification-phase `skipped_type` concept
  (unrecognized file extensions) is unrelated and untouched.

## Verification

- Unit tests in `archive.rs`: a zip containing a real file plus
  `__MACOSX/._realfile` and a non-zip-valid `__MACOSX/._nested.zip`
  expands with zero archive failures and the AppleDouble entries aren't
  returned as members; a member whose extraction would exceed `ratio ×
  this_archive's_compressed_size` is dropped and tallied, while a large
  legitimately-compressed member under the ratio is not capped.
- Integration test in `deduplicate/worker.rs`: an archive with one dropped
  member is excluded from `.processed` after the run (confirms taint
  propagation), while a fully-clean archive is checkpointed as before.
- `mise run ci` clean.
- Manual, on a repro bucket: confirm `pigeon.jsonl`'s archive warnings
  carry `spans[].command == "job.deduplicate"`; confirm a dropped-member
  event is reflected in the printed summary and flips the exit code;
  confirm upload-phase log lines carry the destination alias, not the
  source.
- After merging, remediate the two real buckets per the steps above and
  confirm the rerun's `result/` count grows by roughly the number of
  previously-dropped unique members, exiting 0 with no archive failures.
