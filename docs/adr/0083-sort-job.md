# ADR-0083: `pigeon job run sort`

- **Author**: Willow Finch ([@noisypigeon](https://github.com/noisypigeon)).
- **Date**: 2026-09-30.
- **Status**: Accepted.

## Context

A new bucket-to-bucket job: the wizard takes a mandatory input bucket and
a mandatory output bucket, downloads the input bucket, flattens every file
into a top-level `<extension>/` folder structure (e.g. `pdf/`, `jpeg/`,
`png/`, `heic/`), handles filename collisions, and uploads the flattened
result to the output bucket — no encryption.

The ask explicitly frames this job as sitting **after** `dedupe`
(ADR-0082): "files are unique even if file names collide." This shapes
the whole design:

- **Collision handling exists for a different reason than in any prior
  job.** Every other dedup-adjacent job in this codebase (`dedupe`,
  `pull-transform`, `email-pull`) disambiguates a name collision only
  *after* first checking whether the colliding file is actually the same
  content (a hash hit merges/discards; a hash miss disambiguates the
  name). `sort` never hash-checks anything — by the stated framing, two
  files landing in the same extension folder with the same basename are
  **known to be genuinely different files** (uniqueness was already
  established upstream), so the *only* correct behavior is to always
  disambiguate the name, never merge or overwrite. This means `sort`
  needs no `ContentIndex`/`Dedup` machinery at all — just
  `core::data::unique_path` applied unconditionally on every collision.
- **No zip-expansion code at all.** `dedupe`'s own output bucket (ADR-0082
  §4) never contains zip containers — zips are always expanded and
  discarded there, only their contents are placed. If `sort` runs against
  `dedupe`'s output (the stated use case), there are no zips to expand.
  This is a genuine simplification over both `pull-transform` and
  `dedupe` — no `archive` module involvement, no dynamically-discovered
  work appearing mid-run.
- **Simpler concurrency model.** Because there's no zip expansion
  generating new queue items mid-run, `sort`'s task list is static and
  known entirely up front from the bucket listing — it doesn't need
  `pull-transform`/`dedupe`'s growable-queue-with-in-flight-counter
  pattern at all. A plain `stream::iter(tasks).buffer_unordered(concurrency)`
  (the same shape `commands/job/upload.rs`'s own `run_upload_phase`
  already uses) is sufficient and simpler.
- **Uploading to the output bucket is mandatory**, not optional like
  `pull-transform`/`dedupe` — `--remote-output` gets the same required
  treatment `--source-bucket` already gets in every bucket-sourced job.
- **No extension canonicalization.** Unlike `pull-transform`'s media
  recoding (which maps every image to one canonical format), `sort`
  organizes by each file's literal, as-found, lowercased extension —
  `.jpg` and `.jpeg` land in separate folders, matching `extension_of`'s
  existing convention.

**This is now the third independent implementation of the "recursively
list a bucket + build a per-extension `TypeSummary` + flat-file
`.processed` checkpoint" pattern** (`pull_transform::manifest`,
`dedupe::manifest`, now `sort::manifest`). This codebase has twice already
hoisted exactly this kind of duplication at the third real consumer
(`commands/job/shared_wizard.rs` at ADR-0074 §0; `commands/job/download.rs`
at ADR-0082 §0, which `sort` itself becomes the third consumer of,
further validating that hoist). **This ADR deliberately does not do the
same hoist for the manifest/checkpoint pattern.** The `download.rs` hoist
was pure code motion with zero on-disk format change; generalizing the
manifest/checkpoint pattern would raise the question of whether to
migrate `pull_transform`'s and `dedupe`'s existing on-disk checkpoint
*locations* too — and doing that risks silently discarding in-progress
resumability state for a run already underway on either job. Filed as a
genuinely-deferred (not permanent-boundary) gap via `mise run adr-issue`
(ADR-0031), not silently dropped.

## Decision

### 1. Command surface

`JobType::Sort { source_bucket: Option<String>, local_output:
Option<PathBuf>, remote_output: Option<String>, concurrency:
Option<usize>, yes: bool }`. `source_bucket` and `remote_output` are both
mandatory in practice (non-interactive without either is a hard error) —
`remote_output`'s CLI type stays `Option<String>` like every other job's
(the flag itself is still optional to *type*, resolved by the wizard same
as `source_bucket`), but its `WizardInput` impl has no "skip upload" path
at all, unlike `shared_wizard::UploadTargetInput`. No `--encryption-key`,
ever. `Observable` arm: `"job.sort"` (ADR-0073).

### 2. New job scaffold

`src/commands/job/sort/{mod,wizard,manifest,worker}.rs`
— no `dedup.rs`, no `archive` involvement at all (Context above). `SortJob
{ source_bucket: BucketConfig, source_secret: String, local_output:
PathBuf, remote: (BucketConfig, String) }` implements `core::job::Job` —
`remote` is a plain tuple, not `Option<...>`, reflecting the
mandatory-upload decision structurally rather than just via a runtime
check.

### 3. Gather phase (own `manifest.rs`)

Third duplicate of the listing/`TypeSummary`/checkpoint pattern (see
Context's hoist discussion). `gather_pending(bucket_config, secret,
staging_dir) -> Result<SortPlan, String>` via `client::list_objects(...,
"", true)` (reused directly, same as `pull-transform`/`dedupe`), builds a
per-extension `TypeSummary`, skips already-`.processed`-checkpointed keys.
Checkpoint lives under `staging_dir` (`local_output/.staging/`), not
`local_output` directly — same leak-avoidance convention ADR-0082
established (`core::data::collect_files` doesn't skip dotfiles/
dot-directories, so this is load-bearing, not cosmetic).

### 4. Fetch phase (concurrent, `worker.rs`)

No growable queue (Context above) — `stream::iter(tasks).map(|task|
download_one(...)).buffer_unordered(concurrency)`. Per task:
`download::check_disk_space` + `download::download_with_retry` (both
reused directly from the ADR-0082 §0 shared `commands/job/download.rs` —
`sort` is the third real consumer of that module, further validating the
hoist) into `local_output/.staging/raw/`, recording `(original_key,
extension, raw_path)`. A download failure is tallied and skipped, not
fatal to the run.

### 5. Placement phase (sequential, `worker.rs`)

Sorted by `original_key` first for reproducible order (same discipline as
every other single-threaded placement pass in this codebase — concurrent
`unique_path` calls against a shared directory would race). For each
downloaded file: `result_dir.join(extension).join(sanitize_filename(basename(original_key)))`,
**always** `unique_path`-disambiguated on any collision (same-run or
across a checkpoint-resumed re-run) — never a hash check, never a merge,
per the Context section's reasoning. `fs::rename`, then checkpoint the
source key. No `ContentIndex`/`Dedup` trait involved anywhere in this job.

### 6. Output layout

`local_output/.staging/raw/` (downloaded originals + `.processed`
checkpoint, never uploaded) and `local_output/result/<extension>/<name>`
(the flattened tree, what the upload phase walks) — same `.staging`/
`result` split ADR-0082 established, same reason.

### 7. Upload phase (concurrent, mandatory)

Reuses `commands/job/upload.rs`'s `pending_upload_tasks`/`run_upload_phase`/
`UploadedIndex` unchanged, `walk_dir`/`key_root` both `result_dir`, always
`encryptor: None`. Unlike every other job, this phase isn't conditional —
it always runs, since `remote` isn't `Option`.

### 8. Wizard flow

Own `SourceBucketInput` (mandatory, mirrors `pull_transform`'s exactly) →
own `LocalOutputInput` → `job.gather()` → print type-summary table
(`commands::print_table`) → bail early if `plan.tasks.is_empty()` → own
**mandatory** `RemoteOutputInput` (new — `shared_wizard::UploadTargetInput`
is the wrong shape here, since its `prompt`/`non_interactive_fallback`
both have a "skip" path; `sort`'s version prompts via
`store.prompt_select_bucket()` directly, no "Upload to a bucket-config?"
confirm gate first, and its `non_interactive_fallback` is `Err(...)`,
mirroring `SourceBucketInput`'s own mandatory pattern) → `ConcurrencyInput`/
`ConfirmInput` (shared, unchanged) → `job.run(...)`. No ffmpeg preflight,
no file-type/zip-expansion selection, no encryption-key resolution.

## Consequences

- `sort` is the leanest bucket-to-bucket job in this crate: no new Cargo
  dependencies, no external binary requirement, no `ContentIndex`/`Dedup`
  usage, no zip-expansion code path.
- `sort`'s output has no guaranteed relationship to `dedupe`'s output
  beyond convention — nothing enforces that `sort`'s source bucket was
  actually produced by a prior `dedupe` run. Pointing `sort` at a bucket
  that *does* contain genuine content duplicates doesn't corrupt
  anything (both copies get placed, disambiguated by name), it just
  doesn't deduplicate them — a silent behavioral difference from running
  `dedupe` first, worth understanding before using `sort` standalone.
- A third, independent copy of the bucket-listing/`TypeSummary`/checkpoint
  pattern now exists, deferred for later generalization (see Context and
  Out of scope) rather than hoisted now.

## Out of scope

- Zip expansion — `dedupe`'s own output never contains zip containers
  (ADR-0082 §4); if a zip somehow appears in the source bucket anyway,
  it's placed under `zip/` as a literal, unopened file like everything
  else, not specially handled.
- Content-hash dedup of any kind — this job explicitly assumes uniqueness
  is already guaranteed by whatever ran before it; a genuine
  content-duplicate landing here just gets placed twice under
  disambiguated names, never merged.
- Extension canonicalization/recoding — "sort" organizes by each file's
  literal, as-found extension only.
- Encryption — never offered, permanent per this ADR.
- Generalizing the bucket-listing/`TypeSummary`/checkpoint pattern now ([#97](https://github.com/noisypigeon/noisypigeon-2/issues/97))
  duplicated three times — deferred via `mise run adr-issue` (ADR-0031),
  not a permanent boundary; see Context for why it isn't done here
  (on-disk checkpoint-location migration risk for two already-shipped
  jobs).
