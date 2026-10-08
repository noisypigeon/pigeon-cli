//! Concurrent download/expand/classify/recode/verify pipeline (ADR-0074
//! §4, hardened to stream everything through disk rather than memory by
//! ADR-0076), a sequential dedup/placement pass (§5, `dedup.rs`), and an
//! optional concurrent upload phase (§6, reusing `commands::job::upload`).

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use indicatif::MultiProgress;
use tracing::Instrument;

use crate::commands::job::download;
use crate::commands::job::email_sync::sink;
use crate::commands::job::upload;
use crate::commands::keyring::bucket::store::BucketConfig;
use crate::core::crypto::Aes256GcmSivEncryptor;
use crate::core::data::ContentIndex;

use super::archive;
use super::dedup::{self, ProcessedFile, PullTransformDedup};
use super::documents;
use super::manifest::{self, PullTask, extension_of};
use super::media::{self, MediaKind, TranscodeTargets};

const RECODE_ATTEMPTS: usize = 2;

/// PDF/OOXML date parsing (`lopdf`/`quick-xml`) needs the whole file in
/// memory -- there's no realistic streaming alternative worth building for
/// either. Below this size that's a non-issue for any real document; at or
/// above it (ADR-0076), date extraction is skipped (falls through to the
/// existing mtime/unknown-date chain) rather than risking an unbounded
/// in-memory parse of a file mislabeled as a document.
const MAX_IN_MEMORY_PARSE_BYTES: u64 = 512 * 1024 * 1024;

/// A `failed` count broken down by which stage the failure happened in
/// (same shape as `email_sync::worker::FailureBreakdown`, ADR-0033).
#[derive(Debug, Default)]
pub(crate) struct FailureBreakdown {
    pub download: usize,
    pub archive: usize,
    pub recode: usize,
    pub classify: usize,
    pub placement: usize,
}

impl FailureBreakdown {
    fn merge(&mut self, other: &FailureBreakdown) {
        self.download += other.download;
        self.archive += other.archive;
        self.recode += other.recode;
        self.classify += other.classify;
        self.placement += other.placement;
    }
}

#[derive(Debug, Default)]
pub(crate) struct PullTransformSummary {
    pub processed: usize,
    pub failed: usize,
    pub failure_breakdown: FailureBreakdown,
    pub duplicates_skipped: usize,
    pub recoded: usize,
    pub recode_fallback_to_original: usize,
    pub uploaded: usize,
    pub unchanged: usize,
    pub upload_failed: usize,
    /// Excluded by ADR-0077's `--file-types` filter -- deliberately not part
    /// of `failed` (nothing went wrong) and never checkpointed for a
    /// depth-0 item, so a later run with a broader filter still sees it.
    pub skipped_type: usize,
    /// Zip members dropped by the per-archive extraction-ratio cap
    /// (ADR-0098) -- already folded into `failed`/`failure_breakdown.archive`
    /// too, since dropped data is a real failure, but broken out here so
    /// the wizard can print it as its own distinct, named count.
    pub dropped_members: usize,
    /// Top-level AppleDouble objects (`._*`) skipped before any
    /// download/open attempt (ADR-0107) -- never counted in `failed`.
    pub skipped_apple_double: usize,
    /// Archives that genuinely failed to open (password-protected, corrupt,
    /// etc.), itemized by key/error (ADR-0107).
    pub archive_failures: Vec<archive::ArchiveFailure>,
}

/// Broad category driving both processing (does this need `ffmpeg`?) and
/// eventual placement.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FileKind {
    Zip,
    Image,
    Video,
    Audio,
    Pdf,
    Ooxml,
    Other,
}

fn classify_extension(key: &str) -> (FileKind, String) {
    let extension = extension_of(key);
    let kind = match extension.as_str() {
        "zip" => FileKind::Zip,
        "jpg" | "jpeg" | "png" | "heic" | "heif" | "gif" | "bmp" | "tiff" | "tif" | "webp" => {
            FileKind::Image
        }
        "mov" | "mp4" | "m4v" | "avi" | "mkv" | "webm" | "3gp" | "3g2" => FileKind::Video,
        "m4a" | "mp3" | "wav" | "flac" | "aac" | "ogg" | "wma" | "caf" => FileKind::Audio,
        "pdf" => FileKind::Pdf,
        "docx" | "xlsx" | "pptx" => FileKind::Ooxml,
        _ => FileKind::Other,
    };
    (kind, extension)
}

/// One item on the shared work queue -- a top-level bucket object not yet
/// downloaded (`source_key: Some`, `path: None`), or a file already on disk
/// (`path: Some`) -- true both for a completed top-level download and for
/// a zip member streamed straight to disk during a parent's expansion
/// (ADR-0076 replaces the original in-memory `bytes` field with this).
/// Only a `depth == 0` item is ever checkpointed -- checkpointing tracks
/// "was this top-level object fully handled," not "was every last nested
/// zip member placed" (ADR-0074 §3).
#[derive(Debug)]
struct QueueItem {
    source_key: Option<String>,
    display_key: String,
    path: Option<PathBuf>,
    depth: u32,
    /// The object's declared size in bytes (from the bucket listing for a
    /// top-level object, or the real streamed size for an extracted zip
    /// member) -- used only to decide whether a download is worth
    /// announcing (ADR-0075) and to size the disk-space preflight check
    /// (ADR-0076), never trusted for correctness.
    size: u64,
    /// The top-level task's own `display_key`, unchanged through every
    /// descendant -- tracks whether *any* descendant of a given root failed
    /// or lost data, so that root can be excluded from the checkpoint
    /// (ADR-0098). Never participates in path computation.
    root_key: String,
}

#[derive(Debug)]
enum FailureCategory {
    Download,
    Archive,
    Classify,
}

#[derive(Debug)]
enum ItemOutcome {
    /// `display_key`/`depth` identify the zip that was expanded (checkpoint
    /// candidate iff `depth == 0`); `members` are queued for the next pass.
    /// `dropped` is how many members this zip lost to the per-archive
    /// extraction-ratio cap (ADR-0098) -- nonzero here means real data was
    /// discarded, counted as a failure and tainting this item's root out of
    /// the checkpoint.
    ZipExpanded {
        display_key: String,
        depth: u32,
        members: Vec<QueueItem>,
        dropped: usize,
    },
    Processed {
        depth: u32,
        file: ProcessedFile,
        recoded: bool,
        fell_back_to_original: bool,
    },
    Failed {
        category: FailureCategory,
    },
    /// Excluded by the `--file-types` filter (ADR-0077) before any
    /// download/expansion work was done -- not a failure, and (unlike
    /// `Processed`/`ZipExpanded`) never contributes to `finished_root_keys`,
    /// so it's never checkpointed either.
    Skipped,
    /// A top-level (or, defensively, nested) AppleDouble object (`._*`)
    /// skipped before any download/open attempt (ADR-0107). Unlike
    /// `Skipped` above, this *is* checkpointed (depth == 0) -- it will
    /// never become relevant to a broader `--file-types` filter, so there's
    /// no reason to ever retry it.
    SkippedAppleDouble {
        depth: u32,
    },
    /// `expand_to_dir` genuinely failed to open this archive (password
    /// required, corrupt, etc.) -- as opposed to the depth-cap/disk-space
    /// preconditions, which stay under `Failed { category: Archive }`
    /// unchanged. Carries the key/error so the caller can itemize it
    /// (ADR-0107).
    ArchiveOpenFailed {
        key: String,
        error: String,
    },
}

/// A fresh, not-yet-existing path under `dir` named by `counter`
/// (monotonically increasing, shared across concurrent workers) plus
/// `extension` -- reserved for `ffmpeg` (or some other tool) to write into
/// directly, rather than pre-creating an empty placeholder file.
fn next_scratch_path(dir: &Path, counter: &AtomicU64, extension: &str) -> Result<PathBuf, String> {
    fs::create_dir_all(dir).map_err(|err| format!("failed to create {}: {err}", dir.display()))?;
    let name = counter.fetch_add(1, Ordering::SeqCst);
    Ok(dir.join(format!("{name:012}.{extension}")))
}

/// Streams `path` through SHA-256 on tokio's blocking-thread pool rather
/// than the calling async task's own runtime worker thread -- CPU-bound,
/// same ADR-0088 precedent `deduplicate/worker.rs` set, generalized to every
/// `download::sha256_file` call site in this module (ADR-0090).
async fn hash_file_blocking(path: PathBuf) -> Result<String, String> {
    match tokio::task::spawn_blocking(move || download::sha256_file(&path)).await {
        Ok(result) => result,
        Err(err) => Err(format!("hash task panicked: {err}")),
    }
}

#[tracing::instrument(
    skip(input_path, scratch_dir, counter, multi_progress),
    fields(key = %display_key)
)]
#[allow(clippy::too_many_arguments)]
async fn process_media(
    display_key: &str,
    extension: &str,
    kind: FileKind,
    input_path: PathBuf,
    scratch_dir: &Path,
    counter: &AtomicU64,
    multi_progress: &MultiProgress,
    transcode_targets: &TranscodeTargets,
) -> Result<(ProcessedFile, bool, bool), String> {
    let probe_result = media::probe(&input_path).await;

    let Ok(before) = probe_result else {
        // Couldn't even probe it -- not confidently this file type despite
        // its extension. No loss of data: keep the original file as-is.
        let scratch_path = next_scratch_path(scratch_dir, counter, extension)?;
        fs::rename(&input_path, &scratch_path).map_err(|err| {
            format!(
                "failed to move {} to {}: {err}",
                input_path.display(),
                scratch_path.display()
            )
        })?;
        let content_hash = hash_file_blocking(scratch_path.clone()).await?;
        return Ok((
            ProcessedFile {
                original_key: display_key.to_string(),
                scratch_path,
                extension: extension.to_string(),
                date: None,
                content_hash,
                is_media: false,
            },
            false,
            true,
        ));
    };

    let (media_kind, date) = match kind {
        FileKind::Image => {
            // CPU-bound (EXIF parsing) -- handed to `spawn_blocking` rather
            // than run inline (ADR-0090, same ADR-0088 precedent).
            let exif_path = input_path.clone();
            let exif_date = tokio::task::spawn_blocking(move || media::exif_date(&exif_path))
                .await
                .unwrap_or(None);
            let date = exif_date.or(before.creation_date);
            let is_screenshot = before
                .width
                .zip(before.height)
                .is_some_and(|(w, h)| media::is_screenshot_resolution(w, h));
            let media_kind = if is_screenshot {
                MediaKind::Screenshot
            } else {
                MediaKind::Photo
            };
            (media_kind, date)
        }
        FileKind::Video => (MediaKind::Video, before.creation_date),
        FileKind::Audio => (MediaKind::Audio, before.creation_date),
        FileKind::Zip | FileKind::Pdf | FileKind::Ooxml | FileKind::Other => {
            unreachable!("process_media is only called for Image/Video/Audio")
        }
    };

    let canonical_extension = media_kind.canonical_extension(transcode_targets);
    let output_path = next_scratch_path(scratch_dir, counter, canonical_extension)?;
    let input_already_matches_target = extension.eq_ignore_ascii_case(canonical_extension);

    // Re-encoding is CPU-bound and slow independent of file size, unlike
    // the cheap mjpeg photo/screenshot path below -- always worth naming
    // (ADR-0075), unlike the size-gated download announcement.
    if matches!(media_kind, MediaKind::Video | MediaKind::Audio) {
        let duration = before
            .duration_secs
            .map(|secs| format!("{secs:.1}s"))
            .unwrap_or_else(|| "unknown duration".to_string());
        let dimensions = before
            .width
            .zip(before.height)
            .map(|(w, h)| format!(", {w}x{h}"))
            .unwrap_or_default();
        let _ = multi_progress.println(format!(
            "Recoding {display_key} ({duration}{dimensions})..."
        ));
    }

    let recode_result = recode_and_verify(
        &input_path,
        &output_path,
        media_kind,
        transcode_targets,
        input_already_matches_target,
        before,
    )
    .await;

    match recode_result {
        Ok(()) => {
            let _ = fs::remove_file(&input_path);
            let content_hash = hash_file_blocking(output_path.clone()).await?;
            Ok((
                ProcessedFile {
                    original_key: display_key.to_string(),
                    scratch_path: output_path,
                    extension: canonical_extension.to_string(),
                    date,
                    content_hash,
                    is_media: true,
                },
                true,
                false,
            ))
        }
        Err(err) => {
            tracing::warn!(
                key = %display_key,
                step = "recode",
                error = %err,
                "recode did not verify after retries, keeping original file"
            );
            let _ = fs::remove_file(&output_path);
            let scratch_path = next_scratch_path(scratch_dir, counter, extension)?;
            fs::rename(&input_path, &scratch_path).map_err(|err| {
                format!(
                    "failed to move {} to {}: {err}",
                    input_path.display(),
                    scratch_path.display()
                )
            })?;
            let content_hash = hash_file_blocking(scratch_path.clone()).await?;
            Ok((
                ProcessedFile {
                    original_key: display_key.to_string(),
                    scratch_path,
                    extension: extension.to_string(),
                    date,
                    content_hash,
                    is_media: date.is_some(),
                },
                false,
                true,
            ))
        }
    }
}

async fn recode_and_verify(
    input_path: &Path,
    output_path: &Path,
    kind: MediaKind,
    transcode_targets: &TranscodeTargets,
    input_already_matches_target: bool,
    before: media::ProbeInfo,
) -> Result<(), String> {
    let mut last_err = None;
    for attempt in 1..=RECODE_ATTEMPTS {
        let outcome = match media::recode(
            input_path,
            output_path,
            kind,
            transcode_targets,
            input_already_matches_target,
        )
        .await
        {
            Ok(()) => media::verify(before, output_path).await,
            Err(err) => Err(err),
        };
        match outcome {
            Ok(()) => return Ok(()),
            Err(err) => {
                tracing::warn!(attempt, attempts = RECODE_ATTEMPTS, error = %err, "recode attempt failed");
                last_err = Some(err);
            }
        }
    }
    Err(last_err.unwrap_or_else(|| "recode failed for an unknown reason".to_string()))
}

/// Reads `path` into memory only when it's under `MAX_IN_MEMORY_PARSE_BYTES`
/// (ADR-0076) -- `lopdf`/`quick-xml` both need in-memory access for date
/// parsing and there's no realistic streaming alternative worth building
/// for either, but a file mislabeled as a document could otherwise be
/// arbitrarily large.
fn read_small_file(path: &Path, max_bytes: u64) -> Result<Option<Vec<u8>>, String> {
    let metadata =
        fs::metadata(path).map_err(|err| format!("failed to stat {}: {err}", path.display()))?;
    if metadata.len() > max_bytes {
        return Ok(None);
    }
    let bytes =
        fs::read(path).map_err(|err| format!("failed to read {}: {err}", path.display()))?;
    Ok(Some(bytes))
}

fn process_document_or_other(
    display_key: &str,
    extension: &str,
    kind: FileKind,
    input_path: PathBuf,
    scratch_dir: &Path,
    counter: &AtomicU64,
) -> Result<(ProcessedFile, bool, bool), String> {
    let small_file = read_small_file(&input_path, MAX_IN_MEMORY_PARSE_BYTES)?;

    let date = match (&small_file, kind) {
        (Some(bytes), FileKind::Pdf) => documents::pdf_date(bytes),
        (Some(bytes), FileKind::Ooxml) => documents::ooxml_date(bytes),
        (None, FileKind::Pdf | FileKind::Ooxml) => {
            tracing::warn!(
                key = %display_key,
                step = "classify",
                "file too large to parse for a date, skipping date extraction"
            );
            None
        }
        (_, FileKind::Other) => None,
        (_, FileKind::Zip | FileKind::Image | FileKind::Video | FileKind::Audio) => {
            unreachable!("process_document_or_other is only called for Pdf/Ooxml/Other")
        }
    };

    let scratch_path = next_scratch_path(scratch_dir, counter, extension)?;
    fs::rename(&input_path, &scratch_path).map_err(|err| {
        format!(
            "failed to move {} to {}: {err}",
            input_path.display(),
            scratch_path.display()
        )
    })?;
    let content_hash = match &small_file {
        Some(bytes) => download::sha256_hex(bytes),
        None => download::sha256_file(&scratch_path)?,
    };

    Ok((
        ProcessedFile {
            original_key: display_key.to_string(),
            scratch_path,
            extension: extension.to_string(),
            date,
            content_hash,
            is_media: false,
        },
        false,
        false,
    ))
}

#[allow(clippy::too_many_arguments)]
async fn process_item(
    bucket_config: &BucketConfig,
    secret: &str,
    item: QueueItem,
    raw_dir: &Path,
    scratch_dir: &Path,
    counter: &Arc<AtomicU64>,
    multi_progress: &MultiProgress,
    announce: &download::DownloadAnnounce,
    allowed_extensions: &HashSet<String>,
    expand_zip_keys: &HashSet<String>,
    transcode_targets: &TranscodeTargets,
) -> (String, ItemOutcome) {
    let root_key = item.root_key.clone();
    let depth = item.depth;

    if archive::is_apple_double_basename(&item.display_key) {
        tracing::debug!(
            key = %item.display_key,
            step = "archive",
            "skipping macOS AppleDouble object"
        );
        if let Some(path) = &item.path {
            let _ = fs::remove_file(path);
        }
        return (root_key, ItemOutcome::SkippedAppleDouble { depth });
    }

    let (kind, extension) = classify_extension(&item.display_key);

    // ADR-0077: excluded types are dropped before any download/expansion
    // work, whether this is a top-level object or something already
    // streamed to disk during a parent zip's expansion.
    if !allowed_extensions.contains(&extension) {
        if let Some(path) = &item.path {
            let _ = fs::remove_file(path);
        }
        return (root_key, ItemOutcome::Skipped);
    }

    // ADR-0077: a zip not selected for expansion is handled exactly like a
    // non-media `Other` file -- hashed and placed/uploaded verbatim, never
    // expanded.
    let kind = if kind == FileKind::Zip && !expand_zip_keys.contains(&item.display_key) {
        FileKind::Other
    } else {
        kind
    };

    let path = match item.path {
        Some(path) => path,
        None => {
            let key = item.source_key.as_deref().unwrap_or(&item.display_key);
            if let Err(err) = download::check_disk_space(raw_dir, item.size) {
                tracing::warn!(key = %item.display_key, step = "download", error = %err, "not enough disk space");
                return (
                    root_key,
                    ItemOutcome::Failed {
                        category: FailureCategory::Download,
                    },
                );
            }
            let raw_path = match next_scratch_path(raw_dir, counter, &extension) {
                Ok(path) => path,
                Err(err) => {
                    tracing::warn!(key = %item.display_key, step = "download", error = %err, "failed to allocate a raw path");
                    return (
                        root_key,
                        ItemOutcome::Failed {
                            category: FailureCategory::Download,
                        },
                    );
                }
            };
            if let Err(err) = download::download_with_retry(
                bucket_config,
                secret,
                key,
                item.size,
                &raw_path,
                announce,
            )
            .await
            {
                tracing::warn!(key = %item.display_key, step = "download", error = %err, "download failed");
                let _ = fs::remove_file(&raw_path);
                return (
                    root_key,
                    ItemOutcome::Failed {
                        category: FailureCategory::Download,
                    },
                );
            }
            crate::observability::metrics::record_phase(
                "pull-transform",
                "download",
                "ok",
                Some(bucket_config.alias.as_str()),
            );
            raw_path
        }
    };

    if kind == FileKind::Zip {
        if depth >= archive::MAX_ZIP_DEPTH {
            tracing::warn!(key = %item.display_key, step = "archive", depth, "zip nesting depth cap reached, not expanding further");
            let _ = fs::remove_file(&path);
            return (
                root_key,
                ItemOutcome::Failed {
                    category: FailureCategory::Archive,
                },
            );
        }
        if let Err(err) = download::check_disk_space(raw_dir, 0) {
            tracing::warn!(key = %item.display_key, step = "archive", error = %err, "not enough disk space to expand");
            let _ = fs::remove_file(&path);
            return (
                root_key,
                ItemOutcome::Failed {
                    category: FailureCategory::Archive,
                },
            );
        }
        // CPU-bound (decompression) -- handed to `spawn_blocking` rather
        // than run inline, same ADR-0088 precedent `deduplicate/worker.rs` set,
        // generalized here (ADR-0090). `spawn_blocking` doesn't propagate
        // the ambient `tracing` span on its own, so it's re-entered inside
        // the closure via the span captured just before spawning
        // (ADR-0098).
        let expand_path = path.clone();
        let expand_raw_dir = raw_dir.to_path_buf();
        let expand_counter = Arc::clone(counter);
        let span = tracing::Span::current();
        let expand_result = tokio::task::spawn_blocking(move || {
            span.in_scope(|| archive::expand_to_dir(&expand_path, &expand_raw_dir, &expand_counter))
        })
        .await;
        let outcome = match expand_result {
            Ok(Ok((raw_members, dropped))) => {
                let _ = fs::remove_file(&path);
                let members = raw_members
                    .into_iter()
                    .map(|member| QueueItem {
                        source_key: None,
                        display_key: format!("{}!{}", item.display_key, member.name),
                        path: Some(member.path),
                        depth: depth + 1,
                        size: member.size,
                        root_key: root_key.clone(),
                    })
                    .collect();
                ItemOutcome::ZipExpanded {
                    display_key: item.display_key,
                    depth,
                    members,
                    dropped,
                }
            }
            Ok(Err(err)) => {
                tracing::warn!(key = %item.display_key, step = "archive", error = %err, "failed to open zip archive");
                let _ = fs::remove_file(&path);
                ItemOutcome::ArchiveOpenFailed {
                    key: item.display_key,
                    error: err,
                }
            }
            Err(err) => {
                tracing::error!(key = %item.display_key, step = "archive", error = %err, "zip expansion task panicked");
                let _ = fs::remove_file(&path);
                ItemOutcome::Failed {
                    category: FailureCategory::Archive,
                }
            }
        };
        return (root_key, outcome);
    }

    let processed = match kind {
        FileKind::Image | FileKind::Video | FileKind::Audio => {
            process_media(
                &item.display_key,
                &extension,
                kind,
                path,
                scratch_dir,
                counter,
                multi_progress,
                transcode_targets,
            )
            .await
        }
        FileKind::Pdf | FileKind::Ooxml | FileKind::Other => {
            // CPU-bound (document parsing/hashing) -- handed to
            // `spawn_blocking` rather than run inline (ADR-0090, same
            // ADR-0088 precedent). Span re-entered inside the closure, same
            // reasoning as the zip-expansion call above (ADR-0098).
            let display_key = item.display_key.clone();
            let extension = extension.clone();
            let scratch_dir = scratch_dir.to_path_buf();
            let counter = Arc::clone(counter);
            let span = tracing::Span::current();
            match tokio::task::spawn_blocking(move || {
                span.in_scope(|| {
                    process_document_or_other(
                        &display_key,
                        &extension,
                        kind,
                        path,
                        &scratch_dir,
                        &counter,
                    )
                })
            })
            .await
            {
                Ok(result) => result,
                Err(err) => Err(format!("classify task panicked: {err}")),
            }
        }
        FileKind::Zip => unreachable!("handled above"),
    };

    let outcome = match processed {
        Ok((file, recoded, fell_back)) => ItemOutcome::Processed {
            depth,
            file,
            recoded,
            fell_back_to_original: fell_back,
        },
        Err(err) => {
            tracing::warn!(key = %item.display_key, step = "classify", error = %err, "failed to process file");
            ItemOutcome::Failed {
                category: FailureCategory::Classify,
            }
        }
    };
    (root_key, outcome)
}

/// Runs the full pull-transform pipeline: lists already come in via `tasks`
/// (from `Job::gather`, already filtered against the `.processed`
/// checkpoint by the wizard); downloads/expands/classifies/recodes/verifies
/// them concurrently at `concurrency`, places the results sequentially
/// (dedup + naming), then uploads (if `remote` is given).
#[allow(clippy::too_many_arguments)]
pub(crate) async fn run_pull_transform_job(
    bucket_config: &BucketConfig,
    secret: &str,
    local_output: &Path,
    tasks: Vec<PullTask>,
    concurrency: usize,
    upload_concurrency: usize,
    remote: Option<(&BucketConfig, &str)>,
    encryptor: Option<&Aes256GcmSivEncryptor>,
    allowed_extensions: HashSet<String>,
    expand_zip_keys: HashSet<String>,
    transcode_targets: TranscodeTargets,
) -> Result<PullTransformSummary, String> {
    crate::observability::metrics::set_macro_phase("pull-transform", false);
    // Everything downloaded/extracted lands here first (ADR-0076); a file
    // only leaves this directory once it's either renamed into
    // `scratch_dir` as final output or deleted (a consumed zip, a
    // superseded recode input). No separate "tmp" directory is needed
    // anymore -- a file is either not-yet-downloaded, raw-on-disk, or
    // final; there's no intermediate in-memory stage to stage around.
    let raw_dir = local_output.join(".staging").join("raw");
    let scratch_dir = local_output.join(".staging").join("scratch");
    fs::create_dir_all(&raw_dir)
        .map_err(|err| format!("failed to create {}: {err}", raw_dir.display()))?;
    let counter = Arc::new(AtomicU64::new(0));

    // One `MultiProgress` spans the whole run -- main phase, placement, and
    // (if uploading) upload -- matching `email_sync::worker`'s own shape
    // (ADR-0075).
    let multi_progress = MultiProgress::new();
    let total = tasks.len() as u64;
    let _ = multi_progress.println(format!("Downloading and processing {total} object(s)..."));
    tracing::info!(
        total,
        "pull-transform: download/expand/hash/recode phase starting"
    );
    let bar = sink::new_progress_bar("pull-transform".to_string(), total, &multi_progress);
    let announce = download::DownloadAnnounce::new(bar.clone());

    let queue: Arc<Mutex<std::collections::VecDeque<QueueItem>>> = Arc::new(Mutex::new(
        tasks
            .into_iter()
            .map(|task| QueueItem {
                source_key: Some(task.key.clone()),
                display_key: task.key.clone(),
                path: None,
                depth: 0,
                size: task.size,
                root_key: task.key,
            })
            .collect(),
    ));
    let in_flight = Arc::new(std::sync::atomic::AtomicUsize::new(0));

    let processed_files = Arc::new(Mutex::new(Vec::<ProcessedFile>::new()));
    let finished_root_keys = Arc::new(Mutex::new(Vec::<String>::new()));
    let failure_breakdown = Arc::new(Mutex::new(FailureBreakdown::default()));
    let recoded_count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let fallback_count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let skipped_type_count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let dropped_members = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    // Same root-tainting mechanism as `deduplicate/worker.rs` (ADR-0098): a
    // root is excluded from the checkpoint if any descendant failed or lost
    // a member to the extraction-ratio cap.
    let tainted_roots = Arc::new(Mutex::new(HashSet::<String>::new()));
    // Top-level AppleDouble objects skipped before any download/open
    // attempt -- checkpointed unconditionally, separately from
    // `finished_root_keys` (ADR-0107; same reasoning as `deduplicate`'s
    // worker.rs).
    let apple_double_skipped_roots = Arc::new(Mutex::new(Vec::<String>::new()));
    // Archives that genuinely failed to open (password-protected, corrupt,
    // etc.), itemized by key/error (ADR-0107).
    let archive_failures = Arc::new(Mutex::new(Vec::<archive::ArchiveFailure>::new()));
    let allowed_extensions = Arc::new(allowed_extensions);
    let expand_zip_keys = Arc::new(expand_zip_keys);
    let command_span = tracing::Span::current();

    let worker_count = concurrency.max(1);
    let mut handles = Vec::with_capacity(worker_count);
    for _ in 0..worker_count {
        let queue = Arc::clone(&queue);
        let in_flight = Arc::clone(&in_flight);
        let processed_files = Arc::clone(&processed_files);
        let finished_root_keys = Arc::clone(&finished_root_keys);
        let failure_breakdown = Arc::clone(&failure_breakdown);
        let recoded_count = Arc::clone(&recoded_count);
        let fallback_count = Arc::clone(&fallback_count);
        let skipped_type_count = Arc::clone(&skipped_type_count);
        let dropped_members = Arc::clone(&dropped_members);
        let tainted_roots = Arc::clone(&tainted_roots);
        let apple_double_skipped_roots = Arc::clone(&apple_double_skipped_roots);
        let archive_failures = Arc::clone(&archive_failures);
        let allowed_extensions = Arc::clone(&allowed_extensions);
        let expand_zip_keys = Arc::clone(&expand_zip_keys);
        let counter = Arc::clone(&counter);
        let bucket_config = bucket_config.clone();
        let secret = secret.to_string();
        let raw_dir = raw_dir.clone();
        let scratch_dir = scratch_dir.clone();
        let multi_progress = multi_progress.clone();
        let announce = announce.clone();
        let bar = bar.clone();

        handles.push(tokio::spawn(
            async move {
                loop {
                    let item = { queue.lock().unwrap().pop_front() };
                    let Some(item) = item else {
                        if in_flight.load(Ordering::SeqCst) == 0 {
                            break;
                        }
                        tokio::time::sleep(Duration::from_millis(10)).await;
                        continue;
                    };
                    in_flight.fetch_add(1, Ordering::SeqCst);

                    let (root_key, outcome) = process_item(
                        &bucket_config,
                        &secret,
                        item,
                        &raw_dir,
                        &scratch_dir,
                        &counter,
                        &multi_progress,
                        &announce,
                        &allowed_extensions,
                        &expand_zip_keys,
                        &transcode_targets,
                    )
                    .await;

                    match outcome {
                        ItemOutcome::ZipExpanded {
                            display_key,
                            depth,
                            members,
                            dropped,
                        } => {
                            crate::observability::metrics::record_phase(
                                "pull-transform",
                                "archive",
                                "ok",
                                Some(bucket_config.alias.as_str()),
                            );
                            bar.inc_length(members.len() as u64);
                            queue.lock().unwrap().extend(members);
                            if depth == 0 {
                                finished_root_keys.lock().unwrap().push(display_key);
                            }
                            if dropped > 0 {
                                failure_breakdown.lock().unwrap().archive += dropped;
                                dropped_members.fetch_add(dropped, Ordering::SeqCst);
                                tainted_roots.lock().unwrap().insert(root_key.clone());
                                crate::observability::metrics::record_phase(
                                    "pull-transform",
                                    "archive",
                                    "failed",
                                    Some(bucket_config.alias.as_str()),
                                );
                            }
                        }
                        ItemOutcome::Skipped => {
                            skipped_type_count.fetch_add(1, Ordering::SeqCst);
                            crate::observability::metrics::record_phase(
                                "pull-transform",
                                "classify",
                                "skipped_type",
                                Some(bucket_config.alias.as_str()),
                            );
                        }
                        ItemOutcome::Processed {
                            depth,
                            file,
                            recoded,
                            fell_back_to_original,
                        } => {
                            if depth == 0 {
                                finished_root_keys
                                    .lock()
                                    .unwrap()
                                    .push(file.original_key.clone());
                            }
                            // `FailureBreakdown.recode` is deliberately never
                            // incremented anywhere (ADR-0093) -- a failed recode
                            // falls back to the original file rather than
                            // failing the item, so "recoded" vs "fallback" are
                            // this phase's real outcomes, not a success/failure
                            // binary.
                            if recoded {
                                recoded_count.fetch_add(1, Ordering::SeqCst);
                                crate::observability::metrics::record_phase(
                                    "pull-transform",
                                    "recode",
                                    "recoded",
                                    Some(bucket_config.alias.as_str()),
                                );
                            }
                            if fell_back_to_original {
                                fallback_count.fetch_add(1, Ordering::SeqCst);
                                crate::observability::metrics::record_phase(
                                    "pull-transform",
                                    "recode",
                                    "fallback",
                                    Some(bucket_config.alias.as_str()),
                                );
                            }
                            processed_files.lock().unwrap().push(file);
                            crate::observability::metrics::record_phase(
                                "pull-transform",
                                "classify",
                                "ok",
                                Some(bucket_config.alias.as_str()),
                            );
                        }
                        ItemOutcome::Failed { category, .. } => {
                            tainted_roots.lock().unwrap().insert(root_key.clone());
                            let mut breakdown = failure_breakdown.lock().unwrap();
                            let phase = match category {
                                FailureCategory::Download => {
                                    breakdown.download += 1;
                                    "download"
                                }
                                FailureCategory::Archive => {
                                    breakdown.archive += 1;
                                    "archive"
                                }
                                FailureCategory::Classify => {
                                    breakdown.classify += 1;
                                    "classify"
                                }
                            };
                            crate::observability::metrics::record_phase(
                                "pull-transform",
                                phase,
                                "failed",
                                Some(bucket_config.alias.as_str()),
                            );
                        }
                        ItemOutcome::SkippedAppleDouble { depth } => {
                            if depth == 0 {
                                apple_double_skipped_roots
                                    .lock()
                                    .unwrap()
                                    .push(root_key.clone());
                            }
                            // Deliberately no failure_breakdown/metrics bump
                            // -- this is not a failure (ADR-0107's whole
                            // point).
                        }
                        ItemOutcome::ArchiveOpenFailed { key, error } => {
                            tainted_roots.lock().unwrap().insert(root_key.clone());
                            failure_breakdown.lock().unwrap().archive += 1;
                            archive_failures
                                .lock()
                                .unwrap()
                                .push(archive::ArchiveFailure { key, error });
                            crate::observability::metrics::record_phase(
                                "pull-transform",
                                "archive",
                                "failed",
                                Some(bucket_config.alias.as_str()),
                            );
                        }
                    }

                    bar.inc(1);
                    in_flight.fetch_sub(1, Ordering::SeqCst);
                }
            }
            .instrument(command_span.clone()),
        ));
    }

    let mut first_panic = None;
    for handle in handles {
        if let Err(err) = handle.await {
            tracing::error!(error = %err, "pull-transform worker task panicked");
            if first_panic.is_none() {
                first_panic = Some(format!("worker task panicked: {err}"));
            }
        }
    }
    if let Some(err) = first_panic {
        return Err(err);
    }
    bar.finish();

    let files = Arc::try_unwrap(processed_files)
        .map_err(|_| "internal error: processed file list still shared".to_string())?
        .into_inner()
        .map_err(|_| "internal error: processed file list lock poisoned".to_string())?;
    let mut finished_root_keys = Arc::try_unwrap(finished_root_keys)
        .map_err(|_| "internal error: finished-key list still shared".to_string())?
        .into_inner()
        .map_err(|_| "internal error: finished-key list lock poisoned".to_string())?;
    let failure_breakdown = Arc::try_unwrap(failure_breakdown)
        .map_err(|_| "internal error: failure breakdown still shared".to_string())?
        .into_inner()
        .map_err(|_| "internal error: failure breakdown lock poisoned".to_string())?;
    let dropped_members = Arc::try_unwrap(dropped_members)
        .map_err(|_| "internal error: dropped-member counter still shared".to_string())?
        .into_inner();
    let tainted_roots = Arc::try_unwrap(tainted_roots)
        .map_err(|_| "internal error: tainted-root set still shared".to_string())?
        .into_inner()
        .map_err(|_| "internal error: tainted-root set lock poisoned".to_string())?;
    let apple_double_skipped_roots = Arc::try_unwrap(apple_double_skipped_roots)
        .map_err(|_| "internal error: apple-double root list still shared".to_string())?
        .into_inner()
        .map_err(|_| "internal error: apple-double root list lock poisoned".to_string())?;
    let archive_failures = Arc::try_unwrap(archive_failures)
        .map_err(|_| "internal error: archive-failure list still shared".to_string())?
        .into_inner()
        .map_err(|_| "internal error: archive-failure list lock poisoned".to_string())?;

    tracing::info!(
        processed = files.len(),
        download_failed = failure_breakdown.download,
        archive_failed = failure_breakdown.archive,
        recode_failed = failure_breakdown.recode,
        classify_failed = failure_breakdown.classify,
        dropped_members,
        skipped_apple_double = apple_double_skipped_roots.len(),
        "pull-transform: download/expand/hash/recode phase complete"
    );

    // `dedup` (the full hash->path `ContentIndex`) and `placed_keys` live
    // only inside this block, so they're dropped here, before the upload
    // phase runs, instead of surviving in `run_pull_transform_job`'s own
    // scope through the whole upload phase afterward (ADR-0090, same
    // ADR-0089 precedent set for `deduplicate`).
    let placement_summary = {
        let mut dedup = PullTransformDedup(ContentIndex::load(
            local_output,
            dedup::CONTENT_HASHES_FILE,
        )?);
        let (placement_summary, placed_keys) =
            dedup::place_files(local_output, files, &mut dedup, &multi_progress);
        let placed_keys: HashSet<String> = placed_keys.into_iter().collect();

        // A root key is only checkpointed once every file it produced
        // (itself, for a non-zip; every extracted member, for a zip)
        // actually finished placement -- a zip whose expansion succeeded
        // but whose *members* never reached `place_files` (worker task
        // panic notwithstanding, which already aborts the whole run above)
        // still only gets checkpointed via this same mechanism, since
        // `finished_root_keys` already only contains depth-0 keys.
        finished_root_keys.retain(|key| {
            !tainted_roots.contains(key) && (placed_keys.contains(key) || is_zip_key(key))
        });
        for key in &finished_root_keys {
            manifest::append_checkpoint(local_output, key)?;
        }
        // Checkpointed unconditionally and separately from
        // `finished_root_keys` above -- same reasoning as `deduplicate`'s
        // worker.rs (ADR-0107).
        for key in &apple_double_skipped_roots {
            manifest::append_checkpoint(local_output, key)?;
        }
        placement_summary
    };

    tracing::info!(
        placed = placement_summary.placed,
        duplicates_skipped = placement_summary.duplicates_skipped,
        placement_failed = placement_summary.failed,
        "pull-transform: placement phase complete"
    );

    let mut summary = PullTransformSummary {
        processed: placement_summary.placed,
        failed: failure_breakdown.download
            + failure_breakdown.archive
            + failure_breakdown.classify
            + placement_summary.failed,
        duplicates_skipped: placement_summary.duplicates_skipped,
        recoded: recoded_count.load(Ordering::SeqCst),
        recode_fallback_to_original: fallback_count.load(Ordering::SeqCst),
        skipped_type: skipped_type_count.load(Ordering::SeqCst),
        dropped_members,
        skipped_apple_double: apple_double_skipped_roots.len(),
        archive_failures,
        ..Default::default()
    };
    summary.failure_breakdown.merge(&failure_breakdown);
    summary.failure_breakdown.placement = placement_summary.failed;

    if let Some((remote_bucket, remote_secret)) = remote {
        tracing::info!(bucket = %remote_bucket.alias, "pull-transform: upload phase starting");
        let upload_summary = upload_result(
            &remote_bucket.alias,
            local_output,
            (remote_bucket, remote_secret),
            encryptor,
            upload_concurrency,
            &multi_progress,
        )
        .await?;
        summary.uploaded = upload_summary.uploaded;
        summary.unchanged = upload_summary.unchanged;
        summary.upload_failed = upload_summary.upload_failed;
    }

    Ok(summary)
}

/// Uploads `local_output` to `remote`, resuming via the existing
/// `.staging/.uploaded` index (ADR-0019/ADR-0024) -- the shared upload tail
/// both `run_pull_transform_job` and `run_upload_only` call, so there's one
/// code path and one resume mechanism between a fresh run and a resumed
/// `--upload-only` one (ADR-0090, same ADR-0089 precedent). Reuses the
/// normal path's exact `pending_upload_tasks(local_output, local_output,
/// local_output, ...)` call shape -- unlike `deduplicate`/`sort`, this job never
/// adopted a separate `result/` subdirectory, so its upload walk already
/// (pre-existing, not introduced here) sweeps up `.processed`/
/// `.content-hashes`/`.uploaded` as literal upload candidates since
/// `core::data::collect_files` doesn't skip dotfiles. Not fixed here --
/// `--upload-only` must match the existing (if imperfect) normal-path
/// behavior, not silently diverge from it.
async fn upload_result(
    label: &str,
    local_output: &Path,
    remote: (&BucketConfig, &str),
    encryptor: Option<&Aes256GcmSivEncryptor>,
    upload_concurrency: usize,
    multi_progress: &MultiProgress,
) -> Result<upload::UploadSummary, String> {
    let (remote_bucket, remote_secret) = remote;
    let (tasks, uploaded_index) = upload::pending_upload_tasks(
        "pull-transform",
        label,
        local_output,
        local_output,
        local_output,
        encryptor.is_some(),
    )?;
    let mut uploaded_indexes = std::collections::HashMap::new();
    uploaded_indexes.insert(
        local_output.to_path_buf(),
        Arc::new(Mutex::new(uploaded_index)),
    );
    Ok(upload::run_upload_phase(
        tasks,
        &uploaded_indexes,
        remote_bucket,
        remote_secret,
        encryptor,
        upload_concurrency,
        multi_progress,
    )
    .await)
}

/// Resumes uploading an already-completed local pull-transform run,
/// skipping the bucket listing/download/classify/recode/placement phases
/// entirely (ADR-0090's `--upload-only`, same shape as `deduplicate`'s
/// ADR-0089 version). Reuses `upload_result`, the same helper
/// `run_pull_transform_job`'s own upload tail calls.
pub(crate) async fn run_upload_only(
    local_output: &Path,
    remote: (&BucketConfig, &str),
    encryptor: Option<&Aes256GcmSivEncryptor>,
    upload_concurrency: usize,
) -> Result<upload::UploadSummary, String> {
    let multi_progress = MultiProgress::new();
    let label = remote.0.alias.clone();
    upload_result(
        &label,
        local_output,
        remote,
        encryptor,
        upload_concurrency,
        &multi_progress,
    )
    .await
}

/// A zip whose expansion succeeded is checkpoint-eligible even though its
/// own key never appears in `place_files`'s "placed" list (a zip is never
/// itself placed -- only its extracted members are, each under their own
/// synthetic `"<zip-key>!<member>"` key).
fn is_zip_key(key: &str) -> bool {
    classify_extension(key).0 == FileKind::Zip
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_extension_recognizes_common_media_and_document_types() {
        assert_eq!(classify_extension("a.jpg").0, FileKind::Image);
        assert_eq!(classify_extension("a.JPEG").0, FileKind::Image);
        assert_eq!(classify_extension("a.mov").0, FileKind::Video);
        assert_eq!(classify_extension("a.m4a").0, FileKind::Audio);
        assert_eq!(classify_extension("a.zip").0, FileKind::Zip);
        assert_eq!(classify_extension("a.pdf").0, FileKind::Pdf);
        assert_eq!(classify_extension("a.docx").0, FileKind::Ooxml);
        assert_eq!(classify_extension("a.txt").0, FileKind::Other);
    }

    #[test]
    fn next_scratch_path_produces_unique_paths() {
        let dir = tempfile::tempdir().unwrap();
        let counter = AtomicU64::new(0);
        let a = next_scratch_path(dir.path(), &counter, "jpg").unwrap();
        let b = next_scratch_path(dir.path(), &counter, "jpg").unwrap();
        assert_ne!(a, b);
    }

    #[test]
    fn read_small_file_returns_bytes_under_the_cap() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.bin");
        fs::write(&path, b"hello").unwrap();

        assert_eq!(read_small_file(&path, 10).unwrap(), Some(b"hello".to_vec()));
    }

    #[test]
    fn read_small_file_returns_none_over_the_cap() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.bin");
        fs::write(&path, b"hello world").unwrap();

        assert_eq!(read_small_file(&path, 5).unwrap(), None);
    }

    /// End-to-end regression coverage for `process_media` against a real
    /// `ffmpeg`-generated clip -- classify -> probe -> recode -> verify,
    /// exactly as `process_item` drives it, now operating on an
    /// already-on-disk path (ADR-0076) rather than in-memory bytes. Skipped
    /// (not failed) if `ffmpeg` isn't on `PATH`, same reasoning as
    /// `media`'s own tests.
    #[tokio::test]
    async fn process_media_recodes_a_real_video_to_mp4() {
        if media::check_ffmpeg_available().await.is_err() {
            eprintln!("skipping: ffmpeg/ffprobe not found on PATH");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("clip.mov");
        let output = tokio::process::Command::new("ffmpeg")
            .args([
                "-y",
                "-loglevel",
                "error",
                "-f",
                "lavfi",
                "-i",
                "testsrc=size=320x240:duration=1:rate=10",
                "-pix_fmt",
                "yuv420p",
            ])
            .arg(&source)
            .output()
            .await
            .unwrap();
        assert!(output.status.success());

        let scratch_dir = dir.path().join("scratch");
        let counter = AtomicU64::new(0);

        let (file, recoded, fell_back) = process_media(
            "clips/clip.mov",
            "mov",
            FileKind::Video,
            source,
            &scratch_dir,
            &counter,
            &MultiProgress::new(),
            &TranscodeTargets::default(),
        )
        .await
        .unwrap();

        assert!(recoded);
        assert!(!fell_back);
        assert_eq!(file.extension, "mp4");
        assert!(file.is_media);
        assert!(file.scratch_path.exists());
        assert!(!file.content_hash.is_empty());
    }

    fn all_extensions(exts: &[&str]) -> HashSet<String> {
        exts.iter().map(|s| s.to_string()).collect()
    }

    #[tokio::test]
    async fn process_item_skips_an_excluded_extension_before_downloading() {
        let dir = tempfile::tempdir().unwrap();
        let raw_dir = dir.path().join("raw");
        let scratch_dir = dir.path().join("scratch");
        let counter = Arc::new(AtomicU64::new(0));

        let bucket_config = BucketConfig {
            alias: "unused".to_string(),
            endpoint: "http://127.0.0.1:1".to_string(),
            bucket: "unused".to_string(),
            access_key_id: "unused".to_string(),
            encryption_key_alias: None,
        };

        let item = QueueItem {
            source_key: Some("photo.pdf".to_string()),
            display_key: "photo.pdf".to_string(),
            path: None,
            depth: 0,
            size: 100,
            root_key: "photo.pdf".to_string(),
        };

        let (_root_key, outcome) = process_item(
            &bucket_config,
            "unused-secret",
            item,
            &raw_dir,
            &scratch_dir,
            &counter,
            &MultiProgress::new(),
            &download::DownloadAnnounce::new(indicatif::ProgressBar::hidden()),
            &all_extensions(&["jpg"]),
            &all_extensions(&[]),
            &TranscodeTargets::default(),
        )
        .await;

        assert!(matches!(outcome, ItemOutcome::Skipped));
        assert!(!raw_dir.exists() || fs::read_dir(&raw_dir).unwrap().next().is_none());
    }

    #[test]
    fn process_item_zip_not_in_expand_set_passes_through_as_other() {
        let dir = tempfile::tempdir().unwrap();
        let raw_dir = dir.path().join("raw");
        fs::create_dir_all(&raw_dir).unwrap();
        let scratch_dir = dir.path().join("scratch");
        let counter = Arc::new(AtomicU64::new(0));

        let zip_path = raw_dir.join("archive.zip");
        fs::write(&zip_path, b"not really a zip, just opaque bytes").unwrap();

        let item = QueueItem {
            source_key: None,
            display_key: "archive.zip".to_string(),
            path: Some(zip_path),
            depth: 0,
            size: 36,
            root_key: "archive.zip".to_string(),
        };

        let bucket_config = BucketConfig {
            alias: "unused".to_string(),
            endpoint: "http://127.0.0.1:1".to_string(),
            bucket: "unused".to_string(),
            access_key_id: "unused".to_string(),
            encryption_key_alias: None,
        };

        let (_root_key, outcome) = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(process_item(
                &bucket_config,
                "unused-secret",
                item,
                &raw_dir,
                &scratch_dir,
                &counter,
                &MultiProgress::new(),
                &download::DownloadAnnounce::new(indicatif::ProgressBar::hidden()),
                &all_extensions(&["zip"]),
                &all_extensions(&[]), // "archive.zip" not selected for expansion
                &TranscodeTargets::default(),
            ));

        match outcome {
            ItemOutcome::Processed { file, .. } => {
                assert_eq!(file.extension, "zip");
                assert!(file.scratch_path.exists());
            }
            ItemOutcome::Failed { .. } => panic!("expected Processed (passthrough), got Failed"),
            ItemOutcome::Skipped => panic!("expected Processed (passthrough), got Skipped"),
            ItemOutcome::ZipExpanded { .. } => {
                panic!("expected Processed (passthrough), got ZipExpanded")
            }
            other => panic!("expected Processed (passthrough), got {other:?}"),
        }
    }

    #[tokio::test]
    async fn process_item_skips_a_top_level_apple_double_zip_without_downloading() {
        let dir = tempfile::tempdir().unwrap();
        let raw_dir = dir.path().join("raw");
        let scratch_dir = dir.path().join("scratch");
        let counter = Arc::new(AtomicU64::new(0));

        let bucket_config = BucketConfig {
            alias: "unused".to_string(),
            endpoint: "http://127.0.0.1:1".to_string(),
            bucket: "unused".to_string(),
            access_key_id: "unused".to_string(),
            encryption_key_alias: None,
        };

        let item = QueueItem {
            source_key: Some("Facebook/KGraysen/._export-part-2.zip".to_string()),
            display_key: "Facebook/KGraysen/._export-part-2.zip".to_string(),
            path: None,
            depth: 0,
            size: 4096,
            root_key: "Facebook/KGraysen/._export-part-2.zip".to_string(),
        };

        let (_root_key, outcome) = process_item(
            &bucket_config,
            "unused-secret",
            item,
            &raw_dir,
            &scratch_dir,
            &counter,
            &MultiProgress::new(),
            &download::DownloadAnnounce::new(indicatif::ProgressBar::hidden()),
            &all_extensions(&["zip"]),
            &all_extensions(&[]),
            &TranscodeTargets::default(),
        )
        .await;

        assert!(matches!(
            outcome,
            ItemOutcome::SkippedAppleDouble { depth: 0 }
        ));
        assert!(!raw_dir.exists() || fs::read_dir(&raw_dir).unwrap().next().is_none());
    }

    #[tokio::test]
    async fn process_item_captures_the_key_and_error_for_a_genuinely_failed_archive() {
        let dir = tempfile::tempdir().unwrap();
        let raw_dir = dir.path().join("raw");
        fs::create_dir_all(&raw_dir).unwrap();
        let scratch_dir = dir.path().join("scratch");
        let counter = Arc::new(AtomicU64::new(0));

        let bucket_config = BucketConfig {
            alias: "unused".to_string(),
            endpoint: "http://127.0.0.1:1".to_string(),
            bucket: "unused".to_string(),
            access_key_id: "unused".to_string(),
            encryption_key_alias: None,
        };

        let zip_path = raw_dir.join("some.zip");
        fs::write(&zip_path, b"not really a zip, just opaque bytes").unwrap();

        let item = QueueItem {
            source_key: None,
            display_key: "some.zip".to_string(),
            path: Some(zip_path),
            depth: 0,
            size: 36,
            root_key: "some.zip".to_string(),
        };

        let (_root_key, outcome) = process_item(
            &bucket_config,
            "unused-secret",
            item,
            &raw_dir,
            &scratch_dir,
            &counter,
            &MultiProgress::new(),
            &download::DownloadAnnounce::new(indicatif::ProgressBar::hidden()),
            &all_extensions(&["zip"]),
            &all_extensions(&["some.zip"]),
            &TranscodeTargets::default(),
        )
        .await;

        match outcome {
            ItemOutcome::ArchiveOpenFailed { key, .. } => assert_eq!(key, "some.zip"),
            other => panic!("expected ArchiveOpenFailed, got {other:?}"),
        }
    }
}
