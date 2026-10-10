# ADR-0112: `pigeon job run transform` (rclone + ffmpeg image transcode)

- **Author**: Willow Graysen ([@noisypigeon](https://github.com/noisypigeon)).
- **Date**: 2026-10-10.
- **Status**: Proposed.

## Context

There is currently no way to batch-transcode images living behind an
arbitrary `rclone` remote (not necessarily one of pigeon's own S3-compatible
`BucketConfig` buckets) into a single normalized, maximum-quality `.jpg`
format. The requested shape:

```
pigeon job run transform --input-file-type=png  --source-path 'source:png/'  --destination-path 'destination:jpg/' --local-output /mnt/data/a --yes --transfers 16 --checkers 32
pigeon job run transform --input-file-type=jpeg ...
pigeon job run transform --input-file-type=heic ...
```

uses `--source-path`/`--destination-path`/`--transfers`/`--checkers` --
these are not pigeon's `BucketConfig`/keyring bucket-alias flags
(`deduplicate`/`pull-transform`'s `--source-bucket`/`--destination-bucket`
convention); they are the exact flag names and semantics of
`pigeon job run rclone copy` (ADR-0101, restructured into `rclone
copy`/`rclone purge` by ADR-0110). `transform` is designed to shell out to
the `rclone` binary for all data movement -- raw `remote:path` strings, no
pigeon bucket-config/keyring involvement -- and to `ffmpeg`/`ffprobe`
locally for the actual transcode, combining the `rclone` job's transfer
mechanics with `deduplicate`'s (ADR-0082) local processing/checkpoint/
placement architecture.

Three input types are in scope for this first iteration: `png`, `jpeg`,
`heic`, all transcoded to full-size `.jpg` with no quality loss beyond what
JPEG encoding itself already imposes. The job and its flags are named
generically (`transform`, `--input-file-type`, not `image-transform`/
`--input-image-type`) since further input/output kinds are expected in
later iterations, out of scope here.

Deduplication must **not** be involved: every output file gets a name that
is unique, and duplicate source content is deliberately retained at
separate destination names -- unlike every other job in this codebase,
which either skips or merges byte-identical content.

## Decision

### 1. Command surface

New `JobType::Transform { .. }` (`src/commands/job/cli.rs`):

| Flag | Type | Resolution |
|---|---|---|
| `--input-file-type` | `Option<String>` -> `InputFileType` | flag -> TTY `Select` prompt -> error (mandatory) |
| `--source-path` | `Option<String>` | flag -> TTY `Input` prompt -> error (mandatory). Raw rclone `remote:path`, never a bucket-config alias |
| `--destination-path` | `Option<String>` | same as `--source-path` |
| `--local-output` | `Option<PathBuf>` | flag -> default under the OS temp directory |
| `--concurrency` | `Option<usize>` | reuse shared `CpuConcurrencyInput` (cores-based default) -- governs the ffmpeg transcode pool only; `rclone` manages its own parallelism |
| `--transfers` | `Option<usize>` | flag -> default 8, passed to both rclone invocations |
| `--checkers` | `Option<usize>` | flag -> default 16, passed to both rclone invocations |
| `--tpslimit` | `Option<usize>` | flag -> `None` (no cap) by default |
| `--report-bucket` | `Option<String>` | reuse shared `ReportBucketInput` (mandatory, ADR-0100) |
| `--yes` | `bool` | reuse shared `ConfirmInput` |

`Observable::command_name()` returns `"job.transform"`; `job_name()`
(unchanged) strips the `"job."` prefix to `"transform"`, used consistently
in the `command` tracing span, every metric label, and the ADR-0100
report-bucket upload prefix.

`--input-file-type` is a small, vetted enum (`InputFileType { Png, Jpeg,
Heic }`, `src/commands/job/transform/format.rs`), not a free string --
mirroring `pull_transform::media::ImageFormat`/`rclone::cli::RcloneAction`'s
precedent. `InputFileType::parse` is case-insensitive and accepts `jpg` as
an alias for `jpeg`. An invalid value errors immediately; it never falls
through to an interactive prompt.

### 2. Three-phase pipeline, each phase reusing existing mechanics

```
Phase A (pull)       rclone copy <source-path> <local-output>/source --include '*.<ext>' ...
Phase B (transcode)  local: ffmpeg transcode (png/heic) or copy-through (jpeg) -> <local-output>/result/
Phase C (push)       rclone copy <local-output>/result <destination-path> ...
```

Phase A and Phase C both shell out to `rclone copy`, filtered server-side to
the single `--input-file-type` extension on the pull (`--include
'*.<ext>' --ignore-case`) -- there is no reason to transfer non-matching
files when the whole run is scoped to one extension. Phase B never touches
the network; it is pure local file I/O plus `ffmpeg`/`ffprobe` subprocess
calls.

### 3. Hoisting the rclone subprocess/polling mechanics into a shared helper

`rclone/worker.rs::run_copy_job`'s subprocess-spawn, stderr-drain, and
`tokio::select!` live-JSON-log-polling loop (ADR-0102) is extracted into a
new sibling module, `src/commands/job/rclone_transfer.rs`:

```rust
pub(crate) async fn run_rclone_copy(
    source: &str,
    destination: &str,
    include_extension: Option<&str>,   // Some("png") appends --include '*.png' --ignore-case
    log_path: &Path,
    transfers: usize,
    checkers: usize,
    tpslimit: Option<usize>,
    job_name: &'static str,            // metric label: "rclone-copy" or "transform"
    phase_label: &'static str,         // metric label: "transfer", "pull", or "push"
) -> Result<rclone_log::RcloneLogSummary, String>
```

`rclone/worker.rs::run_copy_job` becomes a thin wrapper calling this with
`include_extension: None, job_name: "rclone-copy", phase_label: "transfer"`,
preserving `rclone copy`'s existing behavior, metric labels, and tests
exactly. `transform`'s Phase A and Phase C each call it directly with their
own `job_name`/`phase_label`. This clears this codebase's own "duplicate
until the third consumer" bar in one PR: three real call sites (`rclone
copy`, `transform` pull, `transform` push) across two jobs, rather than a
second near-identical copy living in `transform`'s own module.

### 4. `jpeg` input is copied through unchanged; `png`/`heic` are transcoded

`src/commands/job/transform/media.rs`:

- `copy_through(input, output)` -- a plain `fs::copy`, used for `jpeg`
  input. Zero re-encode, zero generational quality loss, since the source
  is already a JPEG.
- `transcode_to_jpg(input, output)` -- used for `png`/`heic` input:
  ```
  ffmpeg -y -loglevel error -i <input> -frames:v 1 -q:v 1 -pix_fmt yuvj444p <output>
  ```
  `-q:v 1` is ffmpeg's mjpeg encoder's highest quality setting (the scale
  runs 1=best .. 31=worst); `-pix_fmt yuvj444p` forces 4:4:4 chroma instead
  of ffmpeg's default 4:2:0 subsampling for JPEG output, avoiding the single
  largest avoidable quality loss in a naive encode beyond quantization
  itself; no `-vf scale=...` or other filter preserves the original pixel
  dimensions exactly. JPEG has no true lossless mode in common practice, so
  "no quality loss" is interpreted as "the best quality a JPEG re-encode can
  achieve," not mathematical losslessness.
- A HEIC input on an `ffmpeg` build without `libheif` support fails
  immediately with ffmpeg's own decoder-missing message. That message is
  folded verbatim into the `Err` this function returns -- there is no
  separate HEIC-capability preflight probe; the first real decode attempt
  *is* the capability check.

### 5. Destination filenames are unique by construction, not collision-detected-and-fixed

`core::data::unique_path` (the `-2`/`-3` suffix-on-existing-name scheme
`deduplicate`/`pull_transform` use) only checks what is already present in
the *local* result directory -- it has no visibility into whatever the
remote `--destination-path` already contains from an earlier run, so it can
only guarantee run-local uniqueness, not global uniqueness.
`transform` does not use `unique_path`. Instead,
`src/commands/job/transform/placement.rs::compute_destination_name` derives
every destination filename deterministically from a hash of
`(--source-path, full relative path under <local_output>/source/)`:

```rust
pub(crate) fn compute_destination_name(source_path: &str, original_relative_path: &str) -> String {
    let stem = sanitize_filename(
        Path::new(original_relative_path)
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("file"),
    );
    let digest = download::sha256_hex(
        format!("{source_path}\u{1}{original_relative_path}").as_bytes(),
    );
    format!("{stem}-{}.jpg", &digest[..16])
}
```

Hashing the *full* relative path (not just the basename) means two files
that only differ by directory (e.g. `screenshots/IMG_0001.png` vs.
`photos/IMG_0001.png`) never collide; including `--source-path` in the hash
input means two different source trees with coincidentally identical
relative paths never collide either. 64 bits of hash makes a collision
probability negligible at this job's realistic scale.

`place_one` computes this name, and if the resulting path already exists on
disk, treats that as a **fatal error**, not something to silently resolve
with a suffix -- at this scale, a collision indicates either a genuine bug
or an astronomically unlikely hash collision, and a verifiably-unique naming
scheme should fail loudly rather than quietly falling back to probabilistic
disambiguation (which would defeat the point of having one). Because the
name no longer depends on probing filesystem state, placement carries none
of `deduplicate::dedup::place_one`'s check-then-create race, and therefore
does not need its own sequential pass the way `deduplicate` does --
`transform`'s per-file pipeline (transcode/copy-through -> place ->
checkpoint-append) runs entirely inside the same concurrent,
`--concurrency`-bounded task per file. This also makes reruns more robust
than a suffix-based scheme: the same source file always maps to the exact
same destination name on every run, so Phase C's `rclone copy` incremental
push-skip lines up perfectly with Phase B's checkpoint.

### 6. Checkpointing only where it is actually needed

- **Phase A (pull)**: no pigeon-level checkpoint. `rclone copy` is already
  incrementally idempotent (skips files matching size/modtime at the
  destination), so a rerun's pull is naturally cheap.
- **Phase B (transcode)**: `<local_output>/.staging/.processed` -- one
  relative-source-path per line, appended only once that file's placement
  under `result/` succeeds (synchronized across concurrent tasks). This is
  the only phase with a side effect that is not naturally rerun-safe on its
  own -- it is therefore the only phase that needs a checkpoint.
- **Phase C (push)**: no pigeon-level checkpoint, same reasoning as Phase A.

A full `transform` rerun with identical flags after any abort is therefore
naturally resumable end-to-end with no dedicated `--upload-only`-style flag,
unlike `deduplicate`/`pull_transform`.

### 7. A transcode failure aborts the whole run, with checkpoint-on-success preserved

Unlike `pull_transform`'s recode-fallback-to-original leniency, any single
`ffmpeg` transcode failure (including an unsupported HEIC file) aborts the
entire `transform` run: in-flight dispatched work is allowed to drain, but
no further files are dispatched, and the job returns a non-zero exit. Every
file placed and checkpointed before the failure stays checkpointed, so a
rerun (e.g. after fixing the local `ffmpeg` build) resumes from exactly
where it stopped via the Phase B checkpoint filter -- fail-fast-overall is
orthogonal to checkpoint-on-success-per-item.

### 8. Report format

A bespoke, tab-separated `<local_output>/transform-report.txt` (not the
generic `Debug`-based report `decrypt_files` uses), one row per file Phase B
attempted:

```
source_relative_path	outcome	destination_filename	detail
IMG_0001.png	transcoded	IMG_0001-3f9a2b7c1d4e5f60.jpg
photo.jpeg	copied_through	photo-a1b2c3d4e5f60718.jpg
bad.heic	failed		ffmpeg: Decoder (codec heic) not found for input stream

2 transcoded, 1 copied through, 1 failed.
```

`detail` is empty on success (the hash-derived name is itself proof of no
collision) or the `ffmpeg` error string on failure. The report sits at
`<local_output>` top level, sibling of `.staging/`/`result/`, so it is never
swept into Phase C's push by `core::data::collect_files`. `result/` holds
only `.jpg` files flat (no per-extension subdirectory -- unlike
`deduplicate`/`pull_transform`, there is only ever one output extension
here).

### 9. Metrics

Phase A/C reuse `run_rclone_copy`'s existing delta-polling:
`pigeon_job_phase_total{pigeon_job="transform", phase="pull"|"push",
outcome=...}`. `pigeon_upload_bytes_total`/`pigeon_upload_outcomes_total`
are emitted for Phase C (push) only -- Phase A is a pull, not an upload, and
is excluded from those two upload-specific metrics. Phase B emits
`record_phase_count("transform", "transcode",
"transcoded"|"copied_through"|"failed", ..., None)` per file (`source_bucket`
sentinel `NO_BUCKET`, since this job has no bucket concept).
`set_macro_phase("transform", false)` at Phase B start,
`set_macro_phase("transform", true)` at Phase C start.

## Consequences

- `transform` depends on **both** `rclone` and `ffmpeg`/`ffprobe` being on
  `PATH` -- a new pairing; every other job needs at most one of the two.
- HEIC support is gated entirely on the local `ffmpeg` build's `libheif`
  support, surfaced only when the first HEIC file is actually decoded --
  there is no separate capability-detection step, and on a build without
  it, the very first `--input-file-type=heic` run fails fast.
- Destination filenames are not human-readable beyond their original stem
  (`IMG_0001-3f9a2b7c1d4e5f60.jpg`, not `IMG_0001.jpg` or
  `IMG_0001-2.jpg`) -- a deliberate trade-off for verifiable, by-construction
  uniqueness instead of probabilistic collision avoidance.
- `transform` needs no `--upload-only`/resume flag at all, unlike
  `deduplicate`/`pull_transform`/`email_sync`/`email_pull` -- every phase of
  this job is independently rerun-safe already (see Decision §6).
- This is a new job; there is nothing to migrate and no existing behavior to
  break.

## Out of scope

- True mathematically lossless JPEG encoding -- not practical/standard;
  "no quality loss" is implemented as the best achievable JPEG quality
  setting (Decision §4).
- Any input/output type beyond `png`/`jpeg`/`heic` -> `.jpg` -- the flags
  (`--input-file-type`, `transform`'s own naming) are deliberately generic
  so a later iteration can extend this surface, but that extension is not
  designed here.
- A forward-looking `--output-file-type` flag hardcoded to accept only
  `jpg` today -- deferred until a second output format actually exists,
  per this codebase's "add when needed" precedent (ADR-0082 §0,
  ADR-0096 §0).
- Any content-based deduplication of transcoded output -- explicitly
  excluded by design; duplicate source content is retained at distinct
  destination names.
- Invoking `rclone purge`/`rclone delete` from within this job -- despite
  drawing on the `rclone` job's "copy/delete" subprocess architecture as a
  design template (ADR-0110), `transform` only ever copies (pull, then
  push); it never deletes source content.
- A dedicated `--upload-only`-style resume flag -- not needed; see
  Decision §6.

## References

Draws on ADR-0082 (`deduplicate`'s download/hash/placement/checkpoint
architecture and two-tree `--local-output` layout), ADR-0101/ADR-0110 (the
`rclone` job's raw-passthrough `--source-path`/`--destination-path` design
and subprocess/JSON-log-polling mechanics), ADR-0074 (`pull_transform`'s
`ffmpeg`/`ffprobe` mechanics, departed from for JPEG quality settings),
ADR-0100 (report/transcript/log upload to a report bucket), and ADR-0090
(CPU-bound-work-vs-network-I/O concurrency split).

## Verification

- `mise run ci` clean (fmt-check + lint + test).
- `rclone/worker.rs`'s existing `rclone copy` tests pass unchanged against
  the hoisted `rclone_transfer::run_rclone_copy` (regression check on the
  hoist).
- New `rclone_transfer.rs` test confirms `include_extension: Some("png")`
  actually excludes a non-matching file during a local-filesystem `rclone
  copy`.
- `transform/placement.rs` unit tests confirm `compute_destination_name` is
  deterministic across repeated calls, that two relative paths sharing a
  basename produce different names, and that `place_one` hard-errors rather
  than silently renaming when forced into a collision.
- `transform/worker.rs` integration-style test: a mixed valid/one-corrupt-
  file source tree run through `run_transform_job` end-to-end (skipped if
  `ffmpeg` is not on `PATH`) confirms the checkpoint contains only
  successfully-placed files after a simulated failure, and that a rerun
  resumes correctly.
- `tests/cli.rs`: `transform --help` lists `--input-file-type`,
  `--source-path`, `--destination-path`, `--local-output`, `--concurrency`,
  `--transfers`, `--checkers`, `--tpslimit`, `--report-bucket`, `--yes`, and
  omits `--source-bucket`/`--encryption-key`/`--upload-concurrency`; missing
  `--input-file-type`/`--source-path`/`--destination-path` and an invalid
  `--input-file-type` value all fail fast non-interactively; missing
  `rclone` or `ffmpeg` on `PATH` fails fast with a clear error before any
  prompt.
- Manual: run `pigeon job run transform --input-file-type=png --source-path
  <local-dir> --destination-path <local-dir> --local-output
  /tmp/pigeon-transform-test --yes` against plain local directories (rclone
  treats a bare path as implicitly local, no `rclone.conf` needed),
  confirming the destination receives correctly-named, full-resolution
  `.jpg` files, `transform-report.txt` lists each file, and rerunning the
  exact same command is a no-op.
