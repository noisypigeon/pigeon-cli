# ADR-0076: harden `pull-transform` against large-object/zip memory exhaustion

- **Author**: Willow Finch ([@noisypigeon](https://github.com/noisypigeon)).
- **Date**: 2026-09-28.
- **Status**: Accepted.
- **Amends**: ADR-0074, ADR-0075.

## Context

A real run against a bucket with zip archives up to 50-100GB (some
apparently containing hundreds of thousands of files each) was `SIGKILL`ed
by the OS after ~6.5 minutes. Reading the ADR-0073 observability log
(`pigeon.jsonl`) for that run confirmed the mechanism precisely: the
process's RSS climbed from baseline to a peak of 6.2GB within about 35
seconds, CPU stayed pegged over 100% (multiple cores busy), and the
process's own disk-I/O counters climbed to ~68GB read over the run's
lifetime -- far more than any single object's real size, the signature of
heavy OS-level swapping under memory pressure, not genuine file reads. No
application-level warning or error was ever logged in that window (SIGKILL
can't be caught, so nothing got a chance to log gracefully on the way out).
This lines up exactly with the terminal transcript: the progress bar's
total jumped from 17,121 to 621,500 right as several multi-GB zips were
being handled -- one zip alone unpacked into roughly 604,000 additional
files.

Root cause, confirmed by reading the current implementation: **the entire
pull-transform pipeline operates on whole objects fully materialized in
memory**, at two compounding points:

1. `bucket::client::get_object` (`src/commands/keyring/bucket/client.rs:147-167`)
   downloads a whole object into one contiguous `Vec<u8>` via
   `.content()?.to_segmented_bytes().await?.to_bytes().to_vec()` -- for a
   50-100GB object, this alone can exceed available RAM regardless of
   what's inside it.
2. `archive::expand` (`src/commands/job/pull_transform/archive.rs:37-57`)
   takes a zip's bytes already in memory, and for *every* entry inside it
   calls `entry.read_to_end(&mut buf)`, collecting every single
   decompressed member into one `Vec<ZipMember>` before returning any of
   them to the caller. A zip with 600,000 members means 600,000
   simultaneously-resident decompressed buffers before the caller ever
   gets to look at (or discard) a single one.

Investigating a fix confirmed good news: **this isn't a limitation of the
`minio` crate** (pinned at `0.4.0`) -- its `GetObjectResponse`/
`ObjectContent` types already wrap the raw `reqwest` response and support
genuinely incremental consumption, including a ready-made
`ObjectContent::to_file(dest_path) -> IoResult<u64>` that streams chunks
straight to disk (with per-chunk checksum verification) and never buffers
the whole body. The buffering in `get_object` today is entirely this
project's own choice of the wrong convenience method
(`.to_segmented_bytes().to_bytes()`, whose own doc comment warns it's
"slow, intended for testing/debugging only, do not use in
performance-critical code").

The fix, therefore, is architectural but narrow: **stream every large
object straight to disk, and never fully materialize a zip's decompressed
contents in memory** -- everything downstream (classification, EXIF/date
extraction, hashing, recoding, placement) already operates on file paths
in most places already (ADR-0074's ffmpeg step already uses temp files);
this ADR closes the two spots that don't.

## Decision

### 1. Streaming download (replaces whole-object-in-memory)

`bucket::client::get_object` is replaced with:
```rust
pub async fn download_object_to_file(
    bucket_config: &BucketConfig,
    secret_key: &str,
    key: &str,
    dest_path: &Path,
) -> Result<u64, String> {
    let client = build_client(bucket_config, secret_key)?;
    let resp = client.get_object(bucket_config.bucket.as_str(), key)
        .map_err(...)?.build().send().await.map_err(...)?;
    resp.content().map_err(...)?
        .to_file(dest_path).await
        .map_err(|err| format!("failed to download '{key}' to {}: {err}", dest_path.display()))
}
```
(`get_object` has exactly one caller in this codebase -- `pull_transform::worker::download` -- so this is a rename-and-change, not an additive API; nothing else downloads objects today.)

`worker.rs::download()` allocates a fresh path under a new `.staging/raw/`
directory (same `next_scratch_path`-style counter already used for
`tmp`/`scratch`) and streams straight into it, returning the path instead
of bytes.

### 2. Streaming zip expansion (replaces whole-zip-in-memory)

`archive::expand` becomes:
```rust
pub(crate) struct ExtractedMember {
    pub name: String,
    pub path: PathBuf,
    pub size: u64,
}

pub(crate) fn expand_to_dir(
    zip_path: &Path,
    raw_dir: &Path,
    counter: &AtomicU64,
    extracted_bytes: &AtomicU64,
) -> Result<Vec<ExtractedMember>, String>
```
Opens `ZipArchive::new(File::open(zip_path)?)` -- `File` satisfies the same
`Read + Seek` bound `Cursor<&[u8]>` did, so this is a drop-in swap of the
*source*, not a different zip-crate API. For each entry, allocates a fresh
path and streams the entry's own `Read` impl into that file in bounded
(e.g. 64 KiB) chunks via a small manual copy loop (`copy_capped`), checking
the shared `extracted_bytes` running total **after actual bytes written**
on every chunk -- not the zip's own declared/claimed size, which a
malicious or corrupted archive can lie about. A member that would push the
run past the cap is truncated and dropped (tallied as a failure for that
one member, not the whole zip or job).

The returned `Vec<ExtractedMember>` costs memory proportional to **member
count**, not content size -- a 600,000-entry zip costs low hundreds of MB
of `String`/`PathBuf` overhead, not tens of gigabytes of file content.

### 3. Everything downstream operates on paths, not bytes

- `QueueItem` drops `bytes: Option<Vec<u8>>` for `path: Option<PathBuf>`:
  an item is either "not yet downloaded" (`source_key: Some`, `path: None`)
  or "already a file on disk" (`path: Some`) -- true for both a completed
  top-level download and an extracted zip member alike.
- `process_media` operates directly on the already-on-disk path -- the
  current "write bytes to a temp input file first" step disappears
  entirely, since the file is already real. `kamadak-exif`'s
  `Reader::read_from_container` takes any `BufRead + Seek`, so
  `BufReader::new(File::open(path)?)` replaces `Cursor<&bytes>` with no
  other change. The recode-failed fallback path becomes a plain
  `fs::rename` into the scratch tree instead of re-writing bytes that were
  never dropped in the first place.
- `process_document_or_other` gains a size gate:
  `const MAX_IN_MEMORY_PARSE_BYTES: u64 = 512 * 1024 * 1024;` (512 MiB,
  generous for any real PDF/docx). Below it, the file is read into memory
  once and that same buffer is used for both date parsing (`lopdf`/
  `quick-xml` both require in-memory access -- there's no realistic
  streaming alternative worth building for them) and hashing. At or above
  it, date extraction is skipped (falls through to the existing
  mtime/unknown-date chain from ADR-0074 -- no behavior change in kind,
  just an added safety rail) and the file is hashed by streaming instead.
- New `fn sha256_file(path: &Path) -> Result<String, String>` streams a
  file through `Sha256` in fixed-size chunks, replacing every
  `fs::read(&path)` + `sha256_hex(&bytes)` call site that previously
  pulled a whole (potentially huge, even post-recode) file into memory
  purely to hash it.

### 4. Disk-space awareness (sysinfo is already a dependency)

Confirmed API: `sysinfo::Disks::new_with_refreshed_list().list()` returns
`&[Disk]`, each with `.mount_point()`/`.available_space()`. Before
starting a download or a zip's expansion, resolve which disk backs
`local_output` (the entry whose mount point is the longest matching
prefix) and check its available space against a safety margin (e.g. the
larger of 2 GiB or 1%); below that, fail *that one item* with a clear
"not enough disk space" error rather than silently filling the disk.

`archive::MAX_TOTAL_EXTRACTED_BYTES`'s role changes with this ADR: it was
implicitly a memory-safety cap before (everything extracted stayed
resident in RAM); now that extraction is disk-streamed, it's purely a
disk-space/zip-bomb guard, and the old 10 GiB default is far too small for
this user's legitimate content (zips that themselves are 50-100GB
compressed clearly extract to more than 10 GiB uncompressed). Raised
substantially (e.g. 500 GiB) now that the live disk-space check above is
the real-time backstop -- the fixed constant only needs to catch a
genuinely pathological compression-ratio zip bomb, not bound ordinary
legitimate extraction size.

## Consequences

- Peak memory no longer scales with any object's or zip's size -- it's
  bounded by a small, fixed number of in-flight copy buffers (per
  concurrent worker) plus the zip-member metadata list (proportional to
  file count, not bytes).
- Disk space becomes the new, real resource constraint this job can
  exhaust -- the new live disk-space check is a direct, proportionate
  response, not a fixed guess.
- `get_object`'s only caller changes signature (download-to-path instead
  of download-to-bytes) -- a breaking change to that function, acceptable
  since it has exactly one call site in this codebase today.
- Every processing function that used to take `bytes: Vec<u8>` now takes
  `path: &Path` -- a mechanical but wide-reaching signature change across
  `worker.rs`/`archive.rs`/`media.rs`/`documents.rs`.
- Losing minio's per-chunk checksum verification is not a concern here:
  this job already computes and dedups on its own SHA-256 hash of the
  final bytes, which is the correctness guarantee actually relied on.

## Out of scope

- Progress visibility *during* a single very large (tens-of-GB) transfer
  -- `to_file` is one opaque streaming call with no chunk-level callback;
  ADR-0075's existing "Downloading `<key>` (`<size>`)..." announcement
  still fires before it starts, but there's no incremental update while
  it's in flight. A real fix would mean hand-rolling the chunk loop via
  minio's lower-level `to_stream()` instead of `to_file()`, which also
  means re-implementing checksum verification by hand -- a reasonable
  future ADR on its own, not bundled into a memory-safety fix.
- A concurrency-aware throttle on how many large transfers run at once
  (now a disk-space/bandwidth-fairness question, not a memory one, once
  this ADR lands) -- deferred until it proves to matter in practice.
- Crash-recovery cleanup of orphaned `.staging/raw` files left behind by a
  killed run -- a known, separate gap, not this crash's root cause.
- Multipart/parallel chunked download of one huge object for throughput --
  `to_file` streams a single GET sequentially; that's a performance
  question, not a correctness one.

## Verification

1. `mise run ci` clean, including rewritten `archive` tests (extraction
   now asserted against real files on disk, not in-memory `Vec<u8>`
   fields), a "declared size lies" test proving `copy_capped` enforces the
   cap against actual streamed bytes rather than the zip header's claim,
   `sha256_file`/`sha256_hex` agreement on identical content, and a test
   confirming document date-parsing is skipped above
   `MAX_IN_MEMORY_PARSE_BYTES`.
2. Manual: re-run against the same real bucket (or a representative
   subset including at least one multi-GB zip with many entries) and
   confirm via the `pigeon.jsonl` `resource_sample` stream that memory
   stays flat/bounded through the run instead of spiking into the
   gigabytes, using the exact same diagnostic approach that found this
   bug in the first place.
