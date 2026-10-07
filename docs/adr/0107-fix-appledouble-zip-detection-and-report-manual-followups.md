# ADR-0107: skip top-level AppleDouble `._*.zip` objects; report manual-followup archive failures by key

- **Author**: Willow Graysen ([@noisypigeon](https://github.com/noisypigeon)).
- **Date**: 2026-10-07.
- **Status**: Proposed.

## Context

Across the same 33-run review (ADR-0104's context), every `"failed to open
zip archive"` WARN line in every archived `pigeon.jsonl` falls into exactly
two buckets:

- **65 occurrences** of `invalid zip archive: invalid Zip archive: Could not
  find EOCD`. Sampling all 65 keys: every single one is a top-level bucket
  object named `._<name>.zip` or `._<name>.ZIP` (e.g.
  `.../Facebook/KGraysen/._export-part-2.zip`,
  `.../MEGA_1/._Bookmarked.zip`) -- macOS AppleDouble resource-fork sidecars
  that merely happen to end in `.zip`. These are harmless, content-free noise,
  not real data.
- **59 occurrences** of `failed to read zip entry 0/1: unsupported Zip
  archive: Password required to decrypt file` -- genuinely password-protected
  zips (e.g. a recurring `project-emails-snapshot-2025-12-18-views.zip`'s
  nested per-account attachment archives). These are real, correctly-counted
  failures: the content inside is permanently inaccessible to an automated
  run and needs a human to supply a password or decide to discard it.

The first bucket is a confirmed regression-in-spirit against ADR-0098, which
added exactly this AppleDouble skip -- but only inside `expand_to_dir`
(`src/commands/job/pull_transform/archive.rs:61-70`, `is_apple_metadata_entry`,
and `:80-91`, `is_skippable_zip_entry`), which filters entries *while
iterating an already-open `ZipArchive`*. That function's own doc comment
(lines 56-60) explicitly describes the failure mode this ADR found -- "a
`._foo.zip` stub gets requeued by the caller as a nested zip purely because
of its name, then fails to open" -- but only as something that happens to a
*member inside an expanding zip*, which `expand_to_dir`'s skip already
prevents (a skipped member is never extracted, so it's never requeued).

What `expand_to_dir`'s skip cannot reach is a `._*.zip` object that is itself
a **top-level bucket object**, never nested inside any other zip (confirmed:
none of the 65 sampled keys contain deduplicate's `!` zip-member separator).
These are queued the same as any other bucket object and reach
`process_item` (`src/commands/job/deduplicate/worker.rs:118-129`), which
computes `let is_zip = extension == "zip";` purely from `extension_of`
(`:128-129`) -- no AppleDouble awareness at this level at all, since
`is_apple_metadata_entry`/`is_skippable_zip_entry` live in `archive.rs` and
are only ever called from inside `expand_to_dir`. The result: these 65
objects are downloaded, an attempt is made to open them as a `ZipArchive`,
and they deterministically fail with "Could not find EOCD" every single run
against the same bucket -- a recurring false failure, not a transient one.

Separately, the 59 genuinely-failed password-protected archives are correctly
counted (not silently dropped, per ADR-0098's own fix), but the only
user-facing surface is a bare count: `transcript.txt`/the printed summary
says e.g. `"51 failed (0 download, 51 archive, 0 hash)"`
(`deduplicate/wizard.rs:261-272`), and `FailureBreakdown`
(`deduplicate/worker.rs:62`) tracks only numeric counters -- no key is ever
retained anywhere in the job's own state. Finding out *which* 51/35/38 (the
per-run counts observed) archives actually need a human to go unlock them
currently means grepping the full shared `pigeon.jsonl` for
`"failed to open zip archive"` and manually filtering out the AppleDouble
noise from the genuine failures -- exactly the two categories this ADR's
investigation had to separate by hand.

## Decision

### 1. Apply the AppleDouble skip before the extension-based zip check, at every level

Extract `is_apple_metadata_entry`'s basename-based check (`._` prefix) into a
form usable on a plain key/basename, not just a zip-internal `entry_name`,
and call it from `process_item` (`deduplicate/worker.rs:128-129`) and its
`pull_transform` equivalent *before* `is_zip`/`extension == "zip"` is
evaluated -- for every `QueueItem`, top-level or nested, not only entries
already inside an open `ZipArchive`. A `._*.zip`/`._*.ZIP` object is treated
as skippable noise (logged at `tracing::debug!`, consistent with ADR-0098's
existing AppleDouble handling) rather than queued for download-and-open.

### 2. Itemize genuinely-failed archives for manual follow-up

Add a `Vec<String>` (or similar) to the run's in-memory state, populated
alongside `FailureBreakdown.archive` wherever `"failed to open zip archive"`
is logged (`deduplicate/worker.rs:238` and the `pull_transform` equivalent),
and surface it as an explicit "needs manual attention" section in
`deduplicate-report.txt` (and the generic `report_upload` summary for other
job types) -- the archive key plus the underlying error (password-protected,
corrupt, etc.) -- instead of only a count. This turns "go grep the shared
log" into "read the report."

## Consequences

- The 65 recurring AppleDouble false failures disappear; `failed`/exit-code
  behavior on affected buckets reflects only genuine problems.
- The remaining genuine failures (e.g. password-protected zips) are now
  individually actionable straight from the run's own report, without
  needing to cross-reference the shared `pigeon.jsonl`.
- No data that was previously recoverable becomes unrecoverable -- AppleDouble
  files were never real content; this only stops wasting a download+open
  attempt and a failure slot on them.

## Out of scope

- Adding any mechanism to actually supply a password for an encrypted zip
  (e.g. a `--zip-password` flag) -- not proposed here; the decision is purely
  about surfacing *which* archives need that kind of manual intervention, not
  automating the intervention itself.
- `.DS_Store`/`.git`/`node_modules` skipping (`is_skippable_zip_entry`,
  ADR-0099) is unaffected -- those are already member-only concerns with no
  observed top-level-object equivalent in this sample.

## Verification

- Unit test: a top-level `QueueItem` with key `._something.zip` is skipped by
  `process_item` without a download/open attempt; a real `something.zip` with
  the same content is still processed normally.
- Unit test: a run with one password-protected zip produces a report
  containing that archive's key under a manual-followup section, not just an
  incremented count.
- Manual: rerun `job run deduplicate` against one of the sampled buckets
  (e.g. `backblaze-computer-snapshots`) and confirm its `archive_failed` count
  drops by exactly the number of AppleDouble `._*.zip` objects it previously
  reported, with the remaining genuine failures itemized in the report.
- `mise run ci` clean.
