# ADR-0089: `dedupe --upload-only` resume + streaming uploads

- **Author**: Willow Finch ([@noisypigeon](https://github.com/noisypigeon)).
- **Date**: 2026-10-02.
- **Status**: Accepted.

## Context

A real `mise run pigeon-release job run dedupe` run was killed by the
kernel OOM killer partway through upload (209,375 of 560,705 files
uploaded). The kill happened at 09:00:52; `dmesg` shows
`anon-rss:15864184kB` on a host with 16GB RAM and no swap.
`/root/.local/share/pigeon/logs/pigeon.jsonl` shows RSS sitting at about
10.3GB for the whole upload phase, then jumping to about 14.4GB as the
sorted upload walk reached `iso/`, then `mbox/` (6.65GB), with `mkv/` (up
to 10.9GB) and `mov/` (up to 30.7GB) still ahead. Two causes:

1. `upload_one` (`src/commands/job/upload.rs:175-226` before this change)
   called `fs::read` on the entire file, then `.clone()`d that buffer into
   every retry attempt — at least 2x the file size per in-flight upload,
   times `--concurrency`. It then did a single-part `put_object`
   (`src/commands/keyring/bucket/client.rs:190-238` before this change),
   which S3 also caps at 5GB regardless.
2. `run_dedupe_job` (`src/commands/job/dedupe/worker.rs:403-448` before
   this change) kept `ContentIndex` (the full hash→path map),
   `merge_records` (the human-readable report — 1.1GB in the real run),
   and `placed_keys` all alive in its own top-level scope through
   `run_upload_phase`'s entire await, instead of dropping them once
   placement was done.

There was also no way to resume. `.staging/.processed` already held all
17,121 source keys, so a plain rerun got zero tasks from `gather_pending`
and exited with "Everything is already up to date." (`dedupe/wizard.rs`),
never reaching the upload phase. The user wanted a flag that skips
download/dedupe/placement and resumes uploading, bundled with the
streaming fix so the resumed run can actually finish.

**A landmine found while implementing the streaming-upload mechanics**,
by reading the `minio` 0.4.0 crate's actual source
(`~/.cargo/registry/.../minio-0.4.0/src/s3/builders/put_object.rs`):
`PutObjectContent::send()`'s `calc_part_info` rejects an *explicit*,
known part size whenever `object_size / part_size` rounds up to a part
count of 0 — i.e. a **zero-byte file**, if an explicit part size is always
passed — with `InvalidPartCount`. The fix: only pass `.part_size(...)`
explicitly when the file actually needs multipart (`size >
UPLOAD_PART_SIZE`); at or under that (including empty files), omit it and
let the crate's own default-branch logic produce a plain single-part PUT,
which is what a plain-MD5 ETag assumes anyway. `minio::s3::builders`
re-exports `ObjectContent` (and `calc_part_info`/`DEFAULT_PART_SIZE`/
`MIN_PART_SIZE`/`MAX_PART_SIZE`/`MAX_MULTIPART_COUNT`, unused here but
confirmed public) via `pub use crate::s3::object_content::*`.
`ObjectContent: From<&Path>` streams a file through `async_std::fs::File`
in 8KiB chunks internally — genuinely non-blocking, no `spawn_blocking`
needed for the upload call itself, only for the hashing this ADR adds.

The `md5` crate (0.8.1, already a direct dependency) has a streaming
`Context` (`.consume()`/`.finalize()` → `Digest([u8; 16])`), which is what
makes a fixed-buffer streamed hash — and the standard S3 multipart-ETag
recipe, `hex(md5(concat(part digests)))-N` — straightforward with no new
dependency.

## Decision

### 1. `--upload-only` flag (`job run dedupe`)

`JobType::Dedupe` gains `#[arg(long)] upload_only: bool`
(`src/commands/job/cli.rs`), threaded through `commands.rs` into
`dedupe::wizard::dispatch`. `dispatch_async` checks it right after
loading the keyring store and, if set, delegates to `dispatch_upload_only`
(`src/commands/job/dedupe/wizard.rs`), which:

- resolves `LocalOutputInput` only — no `SourceBucketInput`, no source
  secret, no `job.gather()`, so no bucket listing or source credentials
  are needed at all;
- validates `<local_output>/.staging/.processed` exists and
  `<local_output>/result/` is non-empty (`upload_only_preflight_ok`); on
  failure, `fail("no completed dedupe run found under …; run without
  --upload-only first")`;
- resolves `UploadTargetInput` — mandatory here, unlike every other job's
  optional upload target: `Ok(None)` (declined, or non-interactive with no
  flag) fails with `"--remote-output is required with --upload-only"`;
- resolves `ConcurrencyInput` and `ConfirmInput` exactly as the normal path
  does;
- calls `worker::run_upload_only(&local_output, (&remote_bucket,
  &remote_secret), concurrency)`, prints `"Uploaded {uploaded},
  {unchanged} unchanged, {upload_failed} upload failed."`, and exits
  `FAILURE_EXIT_CODE` if any failed. Never touches `dedupe-report.txt` or
  `.processed`.

The existing upload tail of `run_dedupe_job` (`pending_upload_tasks` +
`uploaded_indexes` + `run_upload_phase`) is extracted into `async fn
upload_result(label, local_output, remote, concurrency, multi_progress) ->
Result<upload::UploadSummary, String>` (`dedupe/worker.rs`). Both
`run_dedupe_job` and the new `run_upload_only` call it, so there's one
code path and one `.staging/.uploaded` resume mechanism between a fresh
run and a resumed one. `run_dedupe_job` keeps passing the source bucket's
alias as `label` (unchanged tracing behavior); `run_upload_only`, which
never touches a source bucket, passes the remote's own alias instead.

### 2. Stream uploads from disk (`upload.rs` / `client.rs`)

`client::upload_if_changed`'s `data: Vec<u8>` parameter becomes `body:
UploadBody`, a new `#[derive(Clone)] pub(crate) enum UploadBody { Path(
PathBuf), Bytes(Bytes) }`. `Bytes` exists because ADR-0025's
AES-256-GCM-SIV encryption is whole-buffer and only ever sees email-sized
files — there's no plaintext-path equivalent to stream for an encrypted
upload.

New constants in `client.rs`: `pub(crate) const UPLOAD_PART_SIZE: u64 = 64
* 1024 * 1024` (pinned explicitly, independent of whatever `minio`'s own
default is or becomes, so pigeon's locally-recomputed ETag can never
silently drift from what the client actually uploads — only ever passed
to `put_object_content` when `size > UPLOAD_PART_SIZE`, per the landmine
above) and `const HASH_CHUNK_BYTES: usize = 8 * 1024 * 1024`.

`expected_etag_for_file(path) -> Result<(String, u64), String>` opens the
file once and streams it through `md5::Context` in `HASH_CHUNK_BYTES`
chunks: at or under `UPLOAD_PART_SIZE`, a plain hex MD5 (matches a
single-PUT's ETag); above it, splits into `UPLOAD_PART_SIZE` parts (last
= remainder), hashes each with its own `Context`, concatenates the raw
16-byte digests, and returns `hex(md5(concatenated))-{part_count}`. Runs
inside `tokio::task::spawn_blocking` — same ADR-0088 precedent: CPU-bound,
shouldn't tie up a runtime worker thread for a 30GB file.

`upload_if_changed` computes `(local_hash, size)` from `body`
(spawn_blocking-streamed for `Path`; a direct `md5::compute` for `Bytes`,
already in memory), compares against the existing object's ETag exactly as
before, and on upload:

- `Bytes`: unchanged behavior — `put_object` with
  `SegmentedBytes::from(bytes)`.
- `Path`: `ObjectContent::from(path.as_path())` via `put_object_content`,
  calling `.part_size(UPLOAD_PART_SIZE)` only when `size >
  UPLOAD_PART_SIZE` before `.build().send().await`. Streams the file,
  multipart-uploads automatically above the threshold, no 5GB cap.

`upload.rs::upload_one` stops calling `fs::read` for the plaintext case:
no encryptor → `UploadBody::Path(task.path.clone())`, built once outside
the retry closure, so each retry re-opens/re-streams the file fresh inside
`upload_if_changed` rather than holding a buffer across attempts; with an
encryptor → read + encrypt once into `Bytes::from(encrypted)` outside the
retry closure, so a retry clones the cheap ref-counted `Bytes` handle, not
the buffer. A new `tracing::info!(file, bytes, "upload started")` at the
top of `upload_one` (stat for `bytes`, no `UploadTask` field change) names
whichever file was actually in flight if a future crash happens mid-upload
again.

### 3. Free dedupe state before upload

In `run_dedupe_job`, the `ContentIndex::load` → `place_and_report` →
`write_report` → `placed_keys`-filter → checkpoint-append sequence is
wrapped in a block expression evaluating to just `placement_summary`;
`dedup_index`, `merge_records`, and the `placed_keys` `HashSet` are all
declared inside that block and so drop at its end, before `upload_result`
runs. In the real run that would have released several GB ahead of the
upload phase.

## Consequences

- `job run dedupe --upload-only --local-output <dir> --remote-output
  <bucket> [--concurrency N]` resumes an interrupted or already-complete
  local run's upload phase without re-downloading, re-hashing, or
  re-placing anything, reusing the same `.staging/.uploaded` resume index
  every other upload phase already relies on.
- Every job sharing `upload.rs`'s upload phase (`dedupe`, `sort`,
  `pull-transform`, `email-pull`; `email-sync` only for its unencrypted
  path, since encryption still routes through `UploadBody::Bytes`) now
  streams plaintext uploads straight from disk instead of buffering the
  whole file, and multipart-uploads anything over 64MiB automatically —
  removing the previous implicit 5GB single-PUT ceiling everywhere, not
  just for `dedupe`.
- `client.rs` gains its first direct use of `minio::s3::builders`'
  higher-level `put_object_content`/`ObjectContent` API, alongside the
  existing low-level `put_object`/`SegmentedBytes` path (kept for the
  `Bytes` case).
- `expected_etag_for_file`'s `tokio::task::spawn_blocking` use is the
  second CPU-bound-work-off-the-runtime-thread application in the
  codebase, following ADR-0088's precedent in `dedupe`'s own hash phase.

## Out of scope

- Streaming encryption for files too large to buffer whole in memory —
  ADR-0025's AES-256-GCM-SIV stays whole-buffer, bounded to email-sized
  files by that ADR's own stated boundary; a plaintext `dedupe`/`sort`/
  `pull-transform` upload never needed it in the first place, since those
  jobs never encrypt. Filed via `mise run adr-issue`, not silently
  dropped.

## Verification

- Unit tests: `--upload-only` parses on `JobType::Dedupe`
  (`job_run_dedupe_help_shows_source_bucket_and_concurrency_flags`);
  `upload_only_preflight_ok` fails on a missing `.processed` or an empty
  `result/` and passes on a completed run (`dedupe/wizard.rs`); a CLI
  integration test confirms the preflight fires before any bucket
  resolution (`job_run_dedupe_upload_only_without_a_completed_run_fails_fast`);
  `pending_upload_tasks` extended with a dedupe-shaped (`walk_dir ==
  key_root`) pre-seeded `.uploaded` case; streamed MD5 matches
  `md5::compute` on small and empty fixtures; the multipart-ETag helper
  matches a hand-computed value for a deterministic 3-part fixture.
- `mise run ci` clean.
- Live resume (the reporting user's own box, not independently
  reproducible here): `mise run pigeon-release job run dedupe
  --upload-only --local-output /mnt/data/pigeon-cli --remote-output
  destination --concurrency 8` against the ~351k pending files
  (560,705 − 209,375), confirming `resource_sample.mem_bytes` stays flat
  and well under 1GB through `mbox/`/`mkv/`/`mov/`, the 30.7GB `.mov`
  uploads successfully, a second `--upload-only` run reports 0 pending,
  and `dedupe-report.txt` is unchanged (still 1.1GB, never touched by
  `--upload-only`).
