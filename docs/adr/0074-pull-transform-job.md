# ADR-0074: `pigeon job run pull-transform`

- **Author**: Willow Finch ([@noisypigeon](https://github.com/noisypigeon)).
- **Date**: 2026-09-27.
- **Status**: Accepted.

## Context

Every existing `pigeon job run` type is either IMAP-sourced (`email-sync`)
or local-disk-sourced (`decrypt-files`) — nothing pulls from an
S3-compatible bucket, normalizes/recompresses the media it finds, and
re-uploads it organized and deduped. The want: point the job at a source
bucket (e.g. an old phone-backup or general "junk drawer" bucket full of
photos, videos, screen recordings, voice memos, zipped exports, and
documents), have it recursively pull everything (including expanding
zips), classify each file, recompress media into one canonical,
size-optimized format per category without perceptible quality loss,
extract a real date for it (EXIF for media, document metadata/content for
everything else), dedup identical content, and lay it out as
`<local-output>/<extension>/...` — then optionally encrypt and upload the
result to a (possibly different) bucket-config, exactly like
`email-sync`'s own upload step.

This closely follows the `Job`/`WizardInput`/wizard-orchestration shape
`email-sync` (ADR-0021/0023/0024/0025) and `decrypt-files` (ADR-0028)
already established — the goal is to extend that same family, reusing as
much of `core::data`/`core::crypto`/the upload pipeline as directly
applies, not re-invent it.

Several design questions had no single obviously-correct answer and were
resolved directly with the user rather than guessed at:

- Actual media recoding shells out to `ffmpeg`/`ffprobe` (a system binary,
  not a cargo dependency) — nothing in the Rust crate ecosystem does real
  audio/video re-encoding, and the ask explicitly requires real
  size/quality tradeoffs, not just renaming.
- Canonical target extensions: **photo & screenshot → `.jpg`, video →
  `.mp4` (H.264/AAC), audio → `.m4a` (AAC)**.
- Screenshot vs. photo, for image files: **aspect ratio/dimensions** —
  match against a table of common device/monitor screen resolutions and
  aspect ratios; anything that doesn't match is a photo.
- Document dates: **metadata first (PDF `/CreationDate`, OOXML
  `docProps/core.xml`), then a text-content date scan, then filesystem
  mtime, then "unknown-date"** — in that order.
- Encoding quality is a **fixed, conservative preset per category** (e.g.
  CRF-based "visually lossless" video, ~256kbps AAC audio, high-quality
  mjpeg for images), not a dynamic per-file quality search — worth flagging
  explicitly since it under-delivers on the literal "determine the most
  optimized" phrasing in the original ask, rather than glossing over that
  gap silently.

## Decision

### 0. Refactor first: extract the wizard inputs this job's third copy would otherwise duplicate

`RemoteOutputInput`/`EncryptionKeyInput` (`email_sync/wizard.rs`) and
`ConcurrencyInput`/`ConfirmInput` (already duplicated near-verbatim across
both `email_sync/wizard.rs` and `decrypt_files/wizard.rs`) would become a
*third* copy-paste under `pull_transform/wizard.rs`. Before writing this
job, hoist all four into a new `commands/job/shared_wizard.rs`:
`UploadTargetInput` (resolves an optional destination bucket-config alias,
same "Upload to a bucket-config?" confirm-then-`Store::prompt_select_bucket`
shape as today's `RemoteOutputInput`), `EncryptionKeyInput` (unchanged
logic, generalized off "is there an upload target" rather than
email-sync-specific naming), `ConcurrencyInput`, `ConfirmInput`. All three
jobs (`email_sync`, `decrypt_files`, `pull_transform`) call these instead
of keeping private copies. This is the same kind of
justified-by-a-second/third-consumer genericization this codebase has
already done twice (ADR-0020's `ContentIndex`, ADR-0023's trait
restructure) — not a speculative abstraction.

### 1. New job scaffold

- `src/commands/job/pull_transform/{mod,wizard,manifest,worker,archive,media,documents,dedup}.rs`
  (mirrors `email_sync`'s module shape, ADR-0008).
- `JobType::PullTransform { source_bucket: Option<String>, local_output:
  Option<PathBuf>, remote_output: Option<String>, encryption_key:
  Option<String>, concurrency: Option<usize>, yes: bool }` added to
  `commands/job/cli.rs`, with an `Observable` arm returning
  `"job.pull-transform"` (ADR-0073) and a new match arm in
  `commands/job/commands.rs` delegating to
  `pull_transform::wizard::dispatch(...)`.
- `PullTransformJob { source_bucket: BucketConfig, source_secret: String,
  local_output: PathBuf, remote: Option<(BucketConfig, String)>, encryptor:
  Option<Aes256GcmSivEncryptor> }` implements `core::job::Job` (`Plan =
  Vec<PullTask>`, `Summary = PullTransformSummary`), same shape as
  `EmailSyncJob`/`DecryptFilesJob`.

### 2. Wizard flow (mirrors ADR-0021's ordering)

`SourceBucketInput` (new, mandatory — reuses `Store::bucket_configs()` +
`Store::prompt_select_bucket()`, `keyring::credentials::get_secret`,
exactly like `email_sync/wizard.rs`'s existing bucket-config resolution)
→ local-output dir (reuse `email_sync::wizard`'s `default_local_output`/
`LocalOutputInput` pattern directly, same `$TMPDIR/pigeon-job` default)
→ **gather()** pulls the manifest (below) and the wizard prints a
file-type summary table (`print_table`, already shared via
`commands/mod.rs`) → `UploadTargetInput`/`EncryptionKeyInput` (shared, per
§0) → `ConcurrencyInput` (shared) → `ConfirmInput` (shared) → `run()`.

### 3. Gather phase: recursive listing + type summary (no downloads yet)

`manifest::gather_pending(bucket_config, secret) -> Result<Vec<PullTask>,
String>` calls `bucket::client::list_objects(bucket_config, secret, "",
true)` (already recursive/paginated) once, and classifies each
`ObjectEntry` by its key's extension into a `TypeSummary { extension,
count, total_bytes }` list for the wizard table. Zip contents aren't known
yet at this point (nothing has been downloaded) — the summary shows `zip`
as its own row, and the post-run summary (like `email-sync`'s
estimated-vs-actual attachment count, ADR-0033 #42) is what reports the
true, post-expansion picture. This is the same "manifest is an estimate,
the real run is authoritative" precedent ADR-0032/0034 already
established, applied to a new source.

A `.processed` checkpoint (source object key → outcome), same role as
`email-sync`'s per-UID checkpoint (ADR-0007/0019), lets a re-run skip
already-fully-handled source keys without re-downloading or
re-transcoding.

### 4. Run phase, part 1 (concurrent): download → expand zips → classify → recode → verify

One `stream::buffer_unordered(concurrency)`-driven worker pool (no
per-identity connection cap needed here, unlike `email-sync` — it's all
one bucket, so this is closer to `decrypt_files::worker`'s simpler shape)
pulling off a shared `Arc<Mutex<VecDeque<PullTask>>>` queue that workers
can also *push onto* — a zip's extracted members become new `PullTask`s
requeued the same way `email-sync`'s batch retries requeue
(`worker.rs`'s `requeue_or_none` precedent), so nested zips (zip-in-zip)
just keep cycling through the same loop until nothing pending remains. A
hard nesting-depth cap (e.g. 10) and a total-extracted-bytes cap per job
guard against zip bombs — enforced in `archive::expand`, tracked in a
shared counter, exceeding either fails that entry (not the whole job).

Per non-zip task:

1. **Download**: `bucket::client::get_object`.
2. **Classify** (`worker::classify`): extension + light magic-byte sniff (a
   small hand-rolled header check, e.g. `PK\x03\x04` for zip/OOXML, `%PDF`
   for PDF, `\xFF\xD8\xFF` for JPEG — no new crate, same
   proportionate-to-the-need spirit as this crate's existing hand-rolled
   `yaml_quote`/`sanitize_filename`) into `Photo | Screenshot | Video |
   Audio | Document(kind) | Other`.
3. **Media date + recode** (`media.rs`, new module):
   - EXIF (photo/screenshot): read `DateTimeOriginal` via a new
     `kamadak-exif` dependency.
   - Video/audio: `ffprobe -print_format json -show_format -show_streams`
     (spawned via `tokio::process::Command`) gives
     `format.tags.creation_time` plus duration/dimensions/codec — used
     both for the date and as the "before" state for verification.
   - Screenshot-vs-photo split (images only): compare dimensions/aspect
     ratio against a small table of common screen resolutions (phone,
     tablet, common monitor sizes) — a match classifies it `Screenshot`,
     otherwise `Photo`. Both still canonicalize to `.jpg`; the split only
     matters for date/verification purposes here (see Out of scope).
   - Recode: `ffmpeg` invocation per category (photo/screenshot → mjpeg
     `.jpg` at a fixed high-quality setting; video → H.264/AAC `.mp4` at a
     fixed CRF; audio → AAC `.m4a`, a cheap remux with no re-encode when the
     source is already AAC-in-M4A). A missing `ffmpeg`/`ffprobe` on `PATH`
     is checked once, up front in `dispatch()`, and fails the whole job
     fast with a clear error rather than failing per-file deep into a run.
   - **Verify**: re-run `ffprobe` on the recoded output, compare duration
     (tolerance) and dimensions (exact) against the pre-recode probe.
     Mismatch → retry the recode (bounded, same `retry_with_backoff` shape
     `email_sync/worker.rs` already has) → if still failing, **fall back to
     keeping the original file byte-for-byte, unmodified** (logged as a
     warning, counted in the summary) — this is the actual "no loss of
     data" guarantee: a file this job can't confidently re-encode is never
     discarded, just left as-is.
4. **Document date** (`documents.rs`, new module): PDF → a new `lopdf`
   dependency reads the trailer's `/CreationDate`; OOXML (docx/xlsx/pptx,
   already zip containers) → the new `zip` dependency plus a new
   `quick-xml` dependency read `docProps/core.xml`'s `dcterms:created`. If
   metadata is absent, extract the document's text (same two crates) and
   pattern-match for a date-like substring; if that also finds nothing,
   fall back to the object's own last-modified time from the bucket
   listing, then to an explicit "unknown-date" bucket.
5. **Hash**: SHA-256 (existing `sha2` dependency) of the *final*
   (post-recode, or post-fallback-to-original) bytes.

Every processed file's outcome (`final_bytes` written to a scratch path,
`extension`, `date`, `hash`, `original_key`) is appended to a shared
`Arc<Mutex<Vec<ProcessedFile>>>` for the next, sequential phase —
mirroring exactly why `email-sync`'s dedup/placement pass is
single-threaded (ADR-0021's addendum: concurrent `unique_path` calls race
on the same target directory; the fix already adopted there is "only
place files from one single-threaded pass").

### 5. Run phase, part 2 (sequential): dedup + placement

New `pull_transform::dedup` wraps `core::data::ContentIndex`/`Dedup`
exactly like `email_sync::dedup::EmailDedup` does, keyed by a new
`.content-hashes` file in the staging dir (SHA-256, not MD5 — the ask was
explicitly SHA-based dedup; `ContentIndex` is hash-agnostic, so no core
change is needed). Sorted by `original_key` for reproducible ordering
across re-runs (same reasoning as `email_sync::dedup::run_dedup_pass`'s
`(mailbox, uid)` sort). For each `ProcessedFile`:

- `dedup.check(hash)` hit → discard the scratch file, count as "duplicate
  skipped" (there's no per-file frontmatter to amend an `also-in:` tag into
  here, unlike email's Markdown output — a plain duplicate is just not
  re-placed).
- Miss → compute the destination path under `<local-output>/<extension>/`:
  - **Media** (photo/screenshot/video/audio): `{date:%Y-%m-%d}-{n}.{ext}`,
    `n` a per-`(extension, date)` counter starting at `1` and incrementing
    for each additional file that date (so a single photo on a given day
    is still `2024-01-26-1.jpg`, not bare `2024-01-26.jpg` — the original
    ask's example pattern was ambiguous on whether the first file omits
    the counter; starting at 1 always keeps the naming scheme uniform and
    avoids a rename when a second file for that day shows up later in the
    same run).
  - **Everything else** (documents, unrecognized/"other" types):
    `core::data::sanitize_filename` on the original basename, run through
    `core::data::unique_path` for a same-run collision (the extracted
    date is recorded for these but doesn't drive the filename — nothing in
    the ask asks documents to be date-renamed, only dated).

  `fs::rename` the scratch file into place, `dedup.commit(hash,
  relative_path)`.

### 6. Run phase, part 3 (concurrent, optional): upload

Directly reuses `email_sync::worker`'s existing upload primitives —
`pending_upload_tasks`, `UploadedIndex`, `upload_key`, `upload_one`,
`run_upload_phase` — against `<local-output>` as the tree to upload and the
wizard-resolved destination `BucketConfig`/secret/`Encryptor`. These
functions are already generic over "a directory of files, a bucket, an
optional encryptor" and don't reference anything email-specific; the
cleanest reuse is lifting them out of `email_sync/worker.rs` into a shared
`commands/job/upload.rs` (used by both jobs) rather than duplicating them a
second time — same "extract on the second real consumer" reasoning as §0.

### 7. New dependencies

```toml
zip = "2"             # verify latest at implementation time -- archive read/extract
kamadak-exif = "0.5"   # verify latest -- EXIF DateTimeOriginal for photos
lopdf = "0.34"         # verify latest -- PDF trailer /CreationDate + text extraction
quick-xml = "0.36"     # verify latest -- OOXML docProps/core.xml + document.xml parsing
```

No pure-Rust image/video/audio codec crate is added — `ffmpeg`/`ffprobe`
(external binaries, checked for on `PATH` at job start, not a Cargo
dependency) do all actual media probing/recoding. `sha2` (content hashing)
and `bytes`/`futures`/`tokio` (concurrency) are already present and cover
everything else.

### 8. Observability (ADR-0073, already in place)

`PullTransformJob`/`JobType::PullTransform` flow through the existing
`Observable` + `run_instrumented` harness automatically. New spans/fields
follow the same convention already established in `email_sync/worker.rs`:
the source bucket alias plays `identity`'s role here; every download/
extract/classify/recode/verify/place/upload step gets a `step` field and
inherits `key`/`file`/`attempt` the same way `email-sync`'s retries do.

## Consequences

- A brand-new external runtime dependency (`ffmpeg`/`ffprobe` on `PATH`)
  is introduced for the first time in this crate — every other job runs
  with nothing beyond the Rust binary itself. The wizard's up-front
  preflight check turns a missing binary into one clear, immediate error
  instead of a confusing per-file failure deep into a long run.
- Four new Cargo dependencies (`zip`, `kamadak-exif`, `lopdf`,
  `quick-xml`), none of which this crate has needed before — all narrowly
  scoped to one concern each (archive extraction, EXIF, PDF, XML), no
  general-purpose "do everything" crate.
- `email_sync`'s upload primitives and both jobs' near-duplicated
  `ConcurrencyInput`/`ConfirmInput`/upload-target wizard inputs move to
  shared modules — a real (if modest) refactor of already-shipped code
  lands alongside this new job, not purely additive.
- Given a fixed, conservative encode preset (not a dynamic quality
  search), some files may end up larger or smaller than an ideal
  per-file-tuned encode would produce — an explicit, accepted simplification
  of the literal "most size optimized" ask, see Out of scope.
- The screenshot/photo split, document-date text-scanning, and the
  "unknown-date" fallback bucket are all heuristics that can misclassify
  or miss a date on genuinely ambiguous input — none of this job's
  behavior is destructive on a miss (worst case: a file lands in
  `unknown-date` or gets classified as a photo instead of a screenshot),
  so a wrong guess here is always correctable after the fact, never a
  data-loss risk.

## Out of scope

- A dynamic, per-file quality/size search (e.g. iterative encode-and-check
  against an SSIM/VMAF target) — fixed conservative presets per category
  only, as agreed above.
- Splitting output folders by category (photo/screenshot/video/audio)
  instead of by extension — the ask specifically asked for
  extension-named top-level folders (`jpg/mov/png/pdf`); the
  screenshot-vs-photo classification exists for date/verification purposes
  but both still land under `jpg/`.
- Legacy binary Office formats (`.doc`/`.xls`/`.ppt`, pre-OOXML) — only
  the modern zip-based OOXML formats get metadata/text date extraction;
  legacy binary formats fall straight through to the mtime/unknown-date
  fallback.
- Re-running against the *same* bucket as both source and destination in
  one invocation (not disallowed, just not a scenario this ADR designs
  around specifically — the existing `.uploaded`/checkpoint mechanisms
  should make it safe either way, but it isn't a called-out test case).
- Any change to `bucket/client.rs`'s S3 API surface — `list_objects`
  (already recursive/paginated) and `get_object` already cover everything
  this job needs to read from a bucket.
