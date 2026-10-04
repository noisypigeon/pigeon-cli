# ADR-0099: fix zip-member key corruption, deduplicate-run observability, and report provenance

- **Author**: Willow Graysen ([@noisypigeon](https://github.com/noisypigeon)).
- **Date**: 2026-10-04.
- **Status**: Accepted.

## Context

A `job run deduplicate` run on 2026-10-04 (poisoned-mega-storage-consolidation,
18:35:48-20:14:31 UTC) was analyzed end to end against `pigeon.jsonl`,
`deduplicate-report.txt`, and the `.staging`/`result` trees. The run completed
correctly -- exit 0, every count reconciled exactly (136,273 `.processed`
roots, 461,686 hashed files = 142,992 placed + 318,694 duplicates, 142,992
`.content-hashes` entries, 142,992 uploaded files matching `result/` exactly)
-- but surfaced one real correctness bug and several observability/quality
gaps, verified directly against the code on `main` at the time of analysis:

1. **A zip member's virtual key leaks into its placed name/extension.** When
   a zip is expanded, each member's `display_key` is built as
   `format!("{}!{}", item.display_key, member.name)`
   (`src/commands/job/deduplicate/worker.rs:223`, identically at
   `src/commands/job/pull_transform/worker.rs:580`). Every consumer that
   derives a filename/extension from this key does so with raw
   `std::path::Path` splitting, which has no idea `!` is a synthetic
   separator: `extension_of` (`src/core/data.rs:387`) finds the *only* dot in
   an extensionless member's key -- the one in the outer zip's own name -- and
   returns `"zip!<member>"` as the "extension." In the real run, 44 files
   landed as `WhatsApp Chat - Raghdan.zip!<member>` under a top-level
   `result/zip!<member>/` folder instead of a sane extension bucket.
2. **The local phase is completely silent in `pigeon.jsonl`, while the
   upload phase is far too loud.** `deduplicate/worker.rs` contains zero
   `tracing::info!` calls -- phase transitions are marked only via Prometheus
   metrics, never a log line, so the log has no record of the ~48-minute
   download/expand/hash/place phase at all. Meanwhile `upload_one`
   (`src/commands/job/upload.rs:218`, `#[tracing::instrument]` with no
   explicit level, defaulting to INFO) auto-emits a span-close event per file
   because the JSON log layer sets `FmtSpan::CLOSE`
   (`src/observability/mod.rs:84`) -- 142,993 of this run's 143,007 total log
   lines (66MB), with zero diagnostic content on the happy path.
3. **~600 permanent "Downloading X (Y MB)..." lines buried the live progress
   bars.** `download::download_with_retry` (`src/commands/job/download.rs:97`)
   calls `multi_progress.println(...)` for every download ≥50MB, and
   `MultiProgress::println` always inserts a permanent scrollback line.
4. **Zip expansion uploads tooling noise as "valuable" data.**
   `.DS_Store`, `.un~` files, `.git` internals, and `node_modules`-style
   vendor code (js/ts/map/AWS-SDK json) were extracted and placed/uploaded
   like any real content -- `is_apple_metadata_entry`
   (`src/commands/job/pull_transform/archive.rs:52`) already unconditionally
   skips `__MACOSX`/AppleDouble noise (ADR-0098) but nothing else.
5. **The merge report can't say where the kept copy came from.**
   `ContentIndex` (`src/core/data.rs:36`) only ever persists
   `<hash> <relative_path>` -- no mapping from a hash to the *original key*
   of the file that was kept, not even in-memory within a single run.
   `MergeRecord.kept_path` is the final placement path, not the source key.
6. Two smaller items were investigated and found to be by-design, not bugs:
   a literal extension containing whitespace (e.g. a `procreate 2` bucket)
   is this codebase's existing no-canonicalization behavior (same precedent
   as `sort`'s ADR-0083), not a defect -- once (1) is fixed, no zip-key case
   produces this anymore either. And 12 upload timeouts in this run, all on
   small files during brief S3 stalls, all recovered via the existing
   flat-timeout retry (ADR-0091) -- changing the timeout scheme wasn't
   justified by anything this run actually showed.

## Decision

### 1. Strip the zip-member separator before deriving a name or extension

Add `after_zip_separator` to `src/core/data.rs`, next to `extension_of`:
returns the portion of a key after its last `!`, or the whole key if there
is none. `extension_of` runs it before splitting on `Path`, which
transitively fixes every caller that classifies by extension (`is_zip_key`,
`classify_extension`). The three places that derive a *filename* from a raw
key independently of `extension_of` -- `deduplicate/dedup.rs`'s `place_one`
and `is_undated_key`, and `pull_transform/dedup.rs`'s `place_one` (non-media
branch) -- are each updated to look at `after_zip_separator(key)` instead of
the raw key before calling `Path::file_name()`.

### 2. Bracket each local phase with an `info!` line; quiet the per-file upload span

`deduplicate/worker.rs::run_deduplicate_job` and
`pull_transform/worker.rs::run_pull_transform_job` each gain four
`tracing::info!` lines -- download/expand/hash phase starting, that phase
complete (with per-category failure/dropped counts), placement phase
complete, and upload phase starting (with the destination bucket alias) --
mirroring the "command started"/"command finished" bracketing ADR-0097
established at the whole-job level, now extended to phase level.
`upload_one`'s `#[tracing::instrument]` gains `level = "debug"`: the default
`--log-level` directive (`warn,pigeon=info`) now drops the per-file
span-close volume to zero by default across all five upload-capable jobs
that share this function, while `--log-level pigeon=debug` still opts back
in. The `warn!` on an upload's actual failure is untouched.

### 3. Replace the permanent download announcement with a transient bar message

Add `{msg}` to the one shared progress-bar template
(`email_sync::sink::new_progress_bar`) -- additive, renders empty unless a
caller sets a message. Add `download::DownloadAnnounce`, wrapping a
`ProgressBar` and an `Arc<AtomicUsize>` in-flight counter: starting a large
download sets the bar's message and increments the counter; finishing
decrements it and only clears the message once it reaches zero, so one
worker's large download finishing doesn't blank another still-in-flight
worker's announcement on the shared bar. `download_with_retry`'s
`&MultiProgress` parameter is replaced with `&DownloadAnnounce`. Threaded
through all three call chains that reach it -- `deduplicate`, `pull_transform`,
and `reduce`'s `process_item`/`run_*_job` functions -- constructed once per
job right after the main bar and cloned into each worker exactly like the
existing `bar.clone()`.

### 4. Skip `.DS_Store`, `.git`, and `node_modules` entries during zip expansion

Extend `is_apple_metadata_entry` into `is_skippable_zip_entry` in
`pull_transform/archive.rs` (shared by both `deduplicate` and
`pull-transform`, since both call `expand_to_dir`): also skip a `.DS_Store`
basename, or any entry with a `.git` or `node_modules` path segment anywhere
in the archive. Matched by segment, not prefix, so a nested occurrence
inside a zipped project tree is caught too. These are intentional exclusions
like the existing AppleDouble skip, not failures -- never counted in
`dropped`.

### 5. Add a `kept_original_key` column to the merge report

`ContentIndex`'s on-disk format moves from 2-field space-separated
(`<hash> <relative_path>`) to 3-field tab-separated
(`<hash>\t<relative_path>\t<original_key>`), with lenient backward-compatible
parsing: a line containing a tab is read as the new format; a line without
one falls back to the old `split_once(' ')` behavior with `original_key`
defaulting to `""`. Two new **inherent** methods --
`check_with_original_key`/`commit_with_key` -- sit alongside the existing
`Dedup`-trait-facing `check`/`commit` (now thin wrappers with an empty
original key), so every other `ContentIndex` consumer (`EmailDedup`,
`PullTransformDedup`, etc.) is unaffected beyond the new empty third field
in their own hash files. `deduplicate/dedup.rs`'s `place_and_report`/
`place_one` -- which already hold the concrete `DeduplicateDedup` type, not
a trait object -- call the new methods directly. `MergeRecord` gains a
`kept_original_key` field; `write_report`'s header gains a fourth column.
This column is blank whenever the kept copy's hash predates this change --
it is never retroactively backfilled, since a hash is only ever committed
once.

### Explicitly declined

- **Extension normalization for literal whitespace** (e.g. `procreate 2`):
  no code change. This is the existing, deliberate no-canonicalization
  design this job's placement inherits from `sort` (ADR-0083); the
  corrupted `zip!...` case this run actually showed is fully fixed by
  Decision 1.
- **Upload timeout tuning** (shorter first attempt / escalating backoff):
  no change to `upload_timeout`/`UPLOAD_RETRIES`. Every timeout this run hit
  already recovered via the existing retry; nothing observed justified the
  added complexity.

## Consequences

- A zip member with no extension of its own (or any member, in general)
  places under its correct extension bucket with its correct name, not a
  `zip!<member>`-corrupted one.
- `pigeon.jsonl` now has a real record of a `deduplicate`/`pull-transform`
  run's local phase, and the upload phase's log volume drops from
  ~143k lines to a handful by default, across every upload-capable job.
- Large-download call-outs no longer bury the live progress bars in
  permanent scrollback.
- `.DS_Store`/`.git`/`node_modules` noise inside a zip is never extracted,
  placed, or uploaded, for both `deduplicate` and `pull-transform`.
- `deduplicate-report.txt` can answer "where did the kept copy actually come
  from" for any duplicate whose kept copy was committed after this change.
- `ContentIndex`-backed hash files (`.content-hashes`,
  `.message-hashes`, `.attachment-hashes`) move to a tab-separated 3-field
  format; files written before this change remain fully readable.

## Out of scope

- Remediating already-written buckets that contain `zip!`-corrupted
  filenames from before this fix -- upload never deletes, and this ADR
  doesn't add a repair pass. A future run against the same source bucket
  will place correctly, but won't retroactively fix or delete prior output.
- Generalizing the four new phase-boundary log lines to `reduce`,
  `email-sync`, or `email-pull` -- their local phases are structured
  differently (lighter-weight, or IMAP-driven progress respectively) and
  this run's analysis didn't surface the same silent-phase gap there.

## Verification

- Unit tests: `extension_of`/`after_zip_separator` with `!`-containing keys;
  `place_and_report`/`place_files` fixtures placing a zip-member key under
  its correct extension/name in both `deduplicate` and `pull_transform`;
  `archive.rs`'s zip-expansion fixture confirming `.DS_Store`/`.git`/
  `node_modules` entries are skipped with `dropped == 0`; `ContentIndex`
  round-trip tests for `commit_with_key`/`check_with_original_key` and
  backward-compatible reads of the old 2-field format; `write_report`'s new
  column; `DownloadAnnounce`'s start/finish message-clearing behavior.
- `mise run ci` clean.
- Manual smoke run against a fixture bucket containing a zip with an
  extensionless root member, a `.DS_Store`/`.git`/`node_modules` entry, and
  a real duplicate: confirm correct placement, confirm the junk entries
  never appear in `result/`, confirm `deduplicate-report.txt`'s
  `kept_original_key` column is populated, confirm `pigeon.jsonl` shows the
  four phase-boundary lines with no per-file upload span spam at the
  default log level, and confirm the terminal shows large-download status
  as an in-place bar message rather than scrolling lines.
