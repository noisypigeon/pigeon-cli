# ADR-0082: `pigeon job run dedupe`

- **Author**: Willow Finch ([@noisypigeon](https://github.com/noisypigeon)).
- **Date**: 2026-09-30.
- **Status**: Accepted.

## Context

A new bucket-to-bucket job: recursively scan a source bucket, inflate
every zip encountered, content-hash every file (including zip-extracted
members) across the *entire* bucket, keep one copy of each distinct file,
generate a human-readable report of what was merged, and upload the
deduped result to a different bucket. Zip containers themselves are never
uploaded — only their inflated contents are.

This closely overlaps the existing `pull-transform` job (ADR-0074, amended
by ADR-0075/0076/0077) — both are bucket-to-bucket jobs that recursively
list a source bucket, expand zips, dedup by content hash, and upload to a
destination bucket. `dedupe` is effectively `pull-transform` minus the
media-recoding/date-extraction/classification machinery (no `ffmpeg`
dependency needed at all), plus a new reporting requirement `pull-transform`
doesn't have. The design below is based on reading `pull-transform`'s
*current* implementation directly — it has evolved since ADR-0074 via
streaming-to-disk downloads and a disk-space preflight check (ADR-0076)
and file-type/zip-selection flags (ADR-0077) — not just its original
proposal.

**On whether content-hash equality is a reliable way to determine
uniqueness**: yes. SHA-256 (matching `pull-transform`'s own ADR-0074
choice) has no known practical collision attacks; the odds of two
different real files colliding by accident are astronomically small for
any realistic bucket size. The one caveat worth stating explicitly: hash
equality proves two files are **byte-identical**, not "semantically the
same" — a re-saved JPEG or a document with a touched timestamp will hash
differently even if a human would call them duplicates. That's exactly
the granularity asked for here ("using the file hash, determines if the
file is a duplicate"), not perceptual/near-duplicate detection, so it's a
correct fit for this job, not a limitation to work around.

Two design questions had no single obviously-correct answer and were
resolved directly with the user rather than guessed at:

- **Encryption**: `dedupe` never offers it, at all — matching
  `email-pull`'s stance (ADR-0081), not `pull-transform`'s optional
  `--encryption-key`.
- **Disk-space-preflight/streaming-download reuse**: `pull_transform::worker`'s
  ADR-0076 disk-space-preflight and streaming-download pattern is exactly
  what `dedupe` needs too, but that module is private and unreachable as
  written. Rather than duplicate it, it's hoisted into a new shared
  `commands/job/download.rs` — mirroring ADR-0074 §0's own precedent
  (`upload.rs` was hoisted out of `email_sync::worker` for the identical
  reason: a second real consumer needing the exact same behavior).
  `pull-transform` is refactored to call the shared module instead of its
  own private copies; this lands as a real, behavior-preserving refactor
  alongside the new job, not just additive code.

Findings from reading the current codebase that shape the rest of this
decision:

- `pull_transform::manifest` (bucket listing, `.processed` checkpoint,
  the `TypeSummary` extension breakdown) is kind-agnostic already but is a
  **private module** (`mod manifest;` in `pull_transform/mod.rs`) —
  unreachable from a sibling `dedupe` module, same "private module" gotcha
  ADR-0081 hit with `email_sync::worker`. Needs its own copy (small, pure
  listing/checkpoint logic, no media coupling).
- `pull_transform::archive` (`expand_to_dir`, `MAX_ZIP_DEPTH`,
  `MAX_TOTAL_EXTRACTED_BYTES`) is `pub(crate)` and **fully kind-agnostic**
  (zip-bomb guards, disk-streamed extraction, ADR-0076) — reused
  **directly**, no duplication, no changes.
- `pull_transform::dedup`'s `place_files`/`ProcessedFile`/
  `PullTransformDedup` are `pub(crate)` and reachable, but shaped around
  date-based media naming and don't expose per-duplicate merge records
  (needed for this job's report requirement) — `dedupe` writes its own
  placement pass, built directly on the same low-level
  `core::data::{ContentIndex, Dedup, unique_path, sanitize_filename}`
  primitives `pull_transform::dedup` and `email_pull::dedup` both already
  use (ADR-0020/0081), so it can track exactly which source key was
  merged into which kept path.
- `bucket::client::{list_objects, download_object_to_file}` already live
  outside `pull_transform` (in `commands/keyring/bucket/client.rs`) and
  are reused directly, same as `pull-transform` already does.
- No new Cargo dependencies needed anywhere — `sha2`, `zip`, `minio`,
  `sysinfo` are all already present from `pull-transform`'s own
  ADR-0074/0076 work. No `ffmpeg`/`ffprobe` requirement either, since
  there's no recoding — a clean simplification versus `pull-transform`.

## Decision

### 0. Refactor first: hoist download primitives into `commands/job/download.rs`

New shared module, hoisted out of `pull_transform::worker`:
`available_disk_space`/`check_disk_space` (ADR-0076's disk-space
preflight, `MIN_FREE_DISK_BYTES` safety margin), `download_with_retry`
(wraps `client::download_object_to_file` + `retry_with_backoff` + the
ADR-0075 ≥50MiB size-announce `println`), `sha256_hex`/`sha256_file`
(streaming hash, disk-based). `pull_transform::worker` is updated to call
the shared versions instead of its own private copies — behavior-preserving,
not a functional change to `pull-transform`.

### 1. Command surface

`JobType::Dedupe { source_bucket: Option<String>, local_output:
Option<PathBuf>, remote_output: Option<String>, concurrency:
Option<usize>, yes: bool }` — modeled on `pull-transform`'s mandatory
`--source-bucket`, optional `--remote-output`/`--local-output`. No
`--encryption-key`. No `--file-types`/`--expand-zips` either — unlike
`pull-transform`, `dedupe`'s entire purpose is exhaustive whole-bucket
comparison, so every file is always processed and every zip is always
expanded; selectivity would work against the feature. `Observable` arm:
`"job.dedupe"` (ADR-0073).

### 2. New job scaffold

`src/commands/job/dedupe/{mod,wizard,manifest,worker,dedup}.rs`
(mirrors `pull_transform`'s module shape, minus `archive`/`media`/
`documents`/`date`, since those are reused directly or unneeded).
`DedupeJob { source_bucket: BucketConfig, source_secret: String,
local_output: PathBuf, remote: Option<(BucketConfig, String)> }`
implements `core::job::Job` — no `encryptor` field at all, a permanent
scoping decision (§0 above), not a temporarily-unused slot.

### 3. Gather phase (own `manifest.rs`)

`gather_pending(bucket_config, secret, local_output) -> Result<DedupePlan, String>`
— own copy of `pull_transform::manifest`'s pattern: `client::list_objects(bucket_config, secret, "", true)`,
builds a `TypeSummary` (extension → count/bytes) for the wizard's pre-run
table, skips already-`.processed`-checkpointed keys. Own
`PROCESSED_FILE_NAME` checkpoint (flat, one key per line, append-only —
same discipline as every other checkpoint dotfile in this codebase).

### 4. Fetch + zip-expansion phase (concurrent, own `worker.rs`)

Same `Arc<Mutex<VecDeque<QueueItem>>>` + requeue-on-zip-expansion pattern
`pull_transform::worker` already uses (own adapted copy — that module is
also private). Per item: `download::check_disk_space` →
`download::download_with_retry` to `local_output/.staging/raw/`. If the
key's extension is `zip`: depth-cap check against
`archive::MAX_ZIP_DEPTH`, another disk-space check, then
`archive::expand_to_dir` (reused directly) — the zip's own downloaded
bytes are discarded, **never hashed or placed**, only its extracted
members (requeued at `depth + 1`, recursing through the same path for
nested zips, same `MAX_TOTAL_EXTRACTED_BYTES` zip-bomb guard). Non-zip
items: `download::sha256_file` (streaming), staged for the placement
pass. A `MultiProgress` bar grows dynamically via `inc_length` as zips
expand — the same ADR-0075 pattern already proven in `pull-transform`.

### 5. Dedup + placement + report phase (sequential, own `dedup.rs`)

Sorted by original key for reproducible order (same reasoning as every
other single-threaded placement pass in this codebase — concurrent
`unique_path` calls against a shared directory would race). For each
hashed file: `ContentIndex::check(hash)` hit → delete the scratch copy,
record a `MergeRecord { duplicate_key, kept_path, content_hash }`, count
a duplicate; miss → place at
`local_output/result/<extension>/<sanitized-name>` (`unique_path`-disambiguated
on a same-run collision), commit the hash. After placement, write a
plain-text report (`local_output/dedupe-report.txt`): one tab-separated
line per `MergeRecord` plus a summary line (`N duplicate(s) removed, M
unique file(s) kept`) — written even when zero duplicates are found, so
the report lives at a predictable, scriptable path every run.

### 6. Output layout

- `local_output/.staging/raw/` — downloaded/extracted files awaiting hash
  and placement; `.processed` checkpoint; `.content-hashes` `ContentIndex`
  file. Never uploaded.
- `local_output/result/<extension>/<name>` — the final deduped tree; this
  is what the upload phase's `walk_dir`/`key_root` point at.
- `local_output/dedupe-report.txt` — sits at the top level, a sibling of
  `.staging`/`result`, **not** nested under `result/` — structurally
  impossible for the upload walk (scoped to `result/` specifically) to
  sweep it into the destination bucket. Deliberate: unlike `pull-transform`
  (which places directly under `local_output`), `dedupe` nests its placed
  tree one level deeper specifically so a human-facing report can live
  alongside it safely.

### 7. Upload phase (concurrent, optional)

Reuses `commands/job/upload.rs`'s `pending_upload_tasks`/`run_upload_phase`/
`UploadedIndex` unchanged, always called with `encryptor: None`. Upload
target stays **optional** (`UploadTargetInput`, shared, same as
`pull-transform`/`email-pull`) — a local-only dedup-and-report run without
a destination bucket configured yet is still a complete, useful run.

### 8. Wizard flow

`SourceBucketInput` (own copy, mandatory — mirrors `pull_transform`'s) →
`LocalOutputInput` (own copy) → `job.gather()` → print the pending
type-summary table (reusing `commands::print_table`) →
`UploadTargetInput` (shared) → `ConcurrencyInput`/`ConfirmInput` (shared)
→ `job.run(...)`. No ffmpeg/ffprobe preflight (nothing to recode) — a
clean simplification versus `pull-transform`'s mandatory preflight check.

## Consequences

- `commands/job/download.rs` becomes a second, symmetric sibling to
  `commands/job/upload.rs` — both now shared, job-agnostic primitives for
  "move bytes between a local disk and a bucket safely." `pull-transform`'s
  own behavior is unchanged, but its `worker.rs` loses its private
  disk-space/streaming-download helpers in favor of the shared ones.
- A second, adapted copy of `pull_transform`'s bucket-listing/checkpoint
  pattern now exists (`dedupe/manifest.rs`) alongside the original, rather
  than one generalized implementation — an explicit, precedent-following
  tradeoff (see Out of scope), not an oversight.
- `dedupe`'s output tree has no cross-file references at all (no
  frontmatter, no `also-in:`, no attachment-list field) — the report file
  is the only place a merge decision is recorded for a human to read.
- No new Cargo dependencies and no external-binary (`ffmpeg`/`ffprobe`)
  requirement — simpler operationally than `pull-transform`.

## Out of scope

- Per-file-type selection / opting individual zips out of expansion
  (`pull-transform`'s ADR-0077 flags) — a permanent design boundary here,
  not a deferred gap: `dedupe`'s entire purpose is exhaustive whole-bucket
  comparison, so selectivity would contradict the feature.
- Any encryption support (§0) — permanent, not deferred.
- Near-duplicate/perceptual dedup (resized images, re-encoded media,
  documents differing only in metadata) — explicitly out of scope per the
  Context section's answer to the hash-reliability question; this job
  detects byte-identical duplicates only.
- Generalizing `pull_transform::manifest`'s listing/checkpoint pattern into ([#97](https://github.com/noisypigeon/noisypigeon-2/issues/97))
  a shared module alongside `download.rs` — `dedupe` is only the second
  consumer of that specific shape; per this codebase's own "duplicate
  until the third consumer" precedent (ADR-0074 §0 itself only hoisted
  `ConcurrencyInput`/`ConfirmInput` at the third consumer), it stays
  duplicated for now.
