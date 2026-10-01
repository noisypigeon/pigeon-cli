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
}

enum FailureCategory {
    Download,
    Archive,
    Classify,
}

enum ItemOutcome {
    /// `display_key`/`depth` identify the zip that was expanded (checkpoint
    /// candidate iff `depth == 0`); `members` are queued for the next pass.
    ZipExpanded {
        display_key: String,
        depth: u32,
        members: Vec<QueueItem>,
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
        let content_hash = download::sha256_file(&scratch_path)?;
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
            let date = media::exif_date(&input_path).or(before.creation_date);
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
            let content_hash = download::sha256_file(&output_path)?;
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
            let content_hash = download::sha256_file(&scratch_path)?;
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
    counter: &AtomicU64,
    extracted_bytes: &AtomicU64,
    multi_progress: &MultiProgress,
    allowed_extensions: &HashSet<String>,
    expand_zip_keys: &HashSet<String>,
    transcode_targets: &TranscodeTargets,
) -> ItemOutcome {
    let depth = item.depth;
    let (kind, extension) = classify_extension(&item.display_key);

    // ADR-0077: excluded types are dropped before any download/expansion
    // work, whether this is a top-level object or something already
    // streamed to disk during a parent zip's expansion.
    if !allowed_extensions.contains(&extension) {
        if let Some(path) = &item.path {
            let _ = fs::remove_file(path);
        }
        return ItemOutcome::Skipped;
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
                return ItemOutcome::Failed {
                    category: FailureCategory::Download,
                };
            }
            let raw_path = match next_scratch_path(raw_dir, counter, &extension) {
                Ok(path) => path,
                Err(err) => {
                    tracing::warn!(key = %item.display_key, step = "download", error = %err, "failed to allocate a raw path");
                    return ItemOutcome::Failed {
                        category: FailureCategory::Download,
                    };
                }
            };
            if let Err(err) = download::download_with_retry(
                bucket_config,
                secret,
                key,
                item.size,
                &raw_path,
                multi_progress,
            )
            .await
            {
                tracing::warn!(key = %item.display_key, step = "download", error = %err, "download failed");
                let _ = fs::remove_file(&raw_path);
                return ItemOutcome::Failed {
                    category: FailureCategory::Download,
                };
            }
            raw_path
        }
    };

    if kind == FileKind::Zip {
        if depth >= archive::MAX_ZIP_DEPTH {
            tracing::warn!(key = %item.display_key, step = "archive", depth, "zip nesting depth cap reached, not expanding further");
            let _ = fs::remove_file(&path);
            return ItemOutcome::Failed {
                category: FailureCategory::Archive,
            };
        }
        if let Err(err) = download::check_disk_space(raw_dir, 0) {
            tracing::warn!(key = %item.display_key, step = "archive", error = %err, "not enough disk space to expand");
            let _ = fs::remove_file(&path);
            return ItemOutcome::Failed {
                category: FailureCategory::Archive,
            };
        }
        return match archive::expand_to_dir(&path, raw_dir, counter, extracted_bytes) {
            Ok(raw_members) => {
                let _ = fs::remove_file(&path);
                let members = raw_members
                    .into_iter()
                    .map(|member| QueueItem {
                        source_key: None,
                        display_key: format!("{}!{}", item.display_key, member.name),
                        path: Some(member.path),
                        depth: depth + 1,
                        size: member.size,
                    })
                    .collect();
                ItemOutcome::ZipExpanded {
                    display_key: item.display_key,
                    depth,
                    members,
                }
            }
            Err(err) => {
                tracing::warn!(key = %item.display_key, step = "archive", error = %err, "failed to open zip archive");
                let _ = fs::remove_file(&path);
                ItemOutcome::Failed {
                    category: FailureCategory::Archive,
                }
            }
        };
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
        FileKind::Pdf | FileKind::Ooxml | FileKind::Other => process_document_or_other(
            &item.display_key,
            &extension,
            kind,
            path,
            scratch_dir,
            counter,
        ),
        FileKind::Zip => unreachable!("handled above"),
    };

    match processed {
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
    }
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
    remote: Option<(&BucketConfig, &str)>,
    encryptor: Option<&Aes256GcmSivEncryptor>,
    allowed_extensions: HashSet<String>,
    expand_zip_keys: HashSet<String>,
    transcode_targets: TranscodeTargets,
) -> Result<PullTransformSummary, String> {
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
    let extracted_bytes = Arc::new(AtomicU64::new(0));

    // One `MultiProgress` spans the whole run -- main phase, placement, and
    // (if uploading) upload -- matching `email_sync::worker`'s own shape
    // (ADR-0075).
    let multi_progress = MultiProgress::new();
    let total = tasks.len() as u64;
    let _ = multi_progress.println(format!("Downloading and processing {total} object(s)..."));
    let bar = sink::new_progress_bar("pull-transform".to_string(), total, &multi_progress);

    let queue: Arc<Mutex<std::collections::VecDeque<QueueItem>>> = Arc::new(Mutex::new(
        tasks
            .into_iter()
            .map(|task| QueueItem {
                source_key: Some(task.key.clone()),
                display_key: task.key,
                path: None,
                depth: 0,
                size: task.size,
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
    let allowed_extensions = Arc::new(allowed_extensions);
    let expand_zip_keys = Arc::new(expand_zip_keys);

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
        let allowed_extensions = Arc::clone(&allowed_extensions);
        let expand_zip_keys = Arc::clone(&expand_zip_keys);
        let counter = Arc::clone(&counter);
        let extracted_bytes = Arc::clone(&extracted_bytes);
        let bucket_config = bucket_config.clone();
        let secret = secret.to_string();
        let raw_dir = raw_dir.clone();
        let scratch_dir = scratch_dir.clone();
        let multi_progress = multi_progress.clone();
        let bar = bar.clone();

        handles.push(tokio::spawn(async move {
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

                let outcome = process_item(
                    &bucket_config,
                    &secret,
                    item,
                    &raw_dir,
                    &scratch_dir,
                    &counter,
                    &extracted_bytes,
                    &multi_progress,
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
                    } => {
                        bar.inc_length(members.len() as u64);
                        queue.lock().unwrap().extend(members);
                        if depth == 0 {
                            finished_root_keys.lock().unwrap().push(display_key);
                        }
                    }
                    ItemOutcome::Skipped => {
                        skipped_type_count.fetch_add(1, Ordering::SeqCst);
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
                        if recoded {
                            recoded_count.fetch_add(1, Ordering::SeqCst);
                        }
                        if fell_back_to_original {
                            fallback_count.fetch_add(1, Ordering::SeqCst);
                        }
                        processed_files.lock().unwrap().push(file);
                    }
                    ItemOutcome::Failed { category, .. } => {
                        let mut breakdown = failure_breakdown.lock().unwrap();
                        match category {
                            FailureCategory::Download => breakdown.download += 1,
                            FailureCategory::Archive => breakdown.archive += 1,
                            FailureCategory::Classify => breakdown.classify += 1,
                        }
                    }
                }

                bar.inc(1);
                in_flight.fetch_sub(1, Ordering::SeqCst);
            }
        }));
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

    let mut dedup = PullTransformDedup(ContentIndex::load(
        local_output,
        dedup::CONTENT_HASHES_FILE,
    )?);
    let (placement_summary, placed_keys) =
        dedup::place_files(local_output, files, &mut dedup, &multi_progress);
    let placed_keys: HashSet<String> = placed_keys.into_iter().collect();

    // A root key is only checkpointed once every file it produced (itself,
    // for a non-zip; every extracted member, for a zip) actually finished
    // placement -- a zip whose expansion succeeded but whose *members*
    // never reached `place_files` (worker task panic notwithstanding, which
    // already aborts the whole run above) still only gets checkpointed via
    // this same mechanism, since `finished_root_keys` already only contains
    // depth-0 keys.
    finished_root_keys.retain(|key| placed_keys.contains(key) || is_zip_key(key));
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
        ..Default::default()
    };
    summary.failure_breakdown.merge(&failure_breakdown);
    summary.failure_breakdown.placement = placement_summary.failed;

    for key in &finished_root_keys {
        manifest::append_checkpoint(local_output, key)?;
    }

    if let Some((remote_bucket, remote_secret)) = remote {
        let (tasks, uploaded_index) = upload::pending_upload_tasks(
            &bucket_config.alias,
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
        let upload_summary = upload::run_upload_phase(
            tasks,
            &uploaded_indexes,
            remote_bucket,
            remote_secret,
            encryptor,
            concurrency,
            &multi_progress,
        )
        .await;
        summary.uploaded = upload_summary.uploaded;
        summary.unchanged = upload_summary.unchanged;
        summary.upload_failed = upload_summary.upload_failed;
    }

    Ok(summary)
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
        let counter = AtomicU64::new(0);
        let extracted_bytes = AtomicU64::new(0);

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
        };

        let outcome = process_item(
            &bucket_config,
            "unused-secret",
            item,
            &raw_dir,
            &scratch_dir,
            &counter,
            &extracted_bytes,
            &MultiProgress::new(),
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
        let counter = AtomicU64::new(0);
        let extracted_bytes = AtomicU64::new(0);

        let zip_path = raw_dir.join("archive.zip");
        fs::write(&zip_path, b"not really a zip, just opaque bytes").unwrap();

        let item = QueueItem {
            source_key: None,
            display_key: "archive.zip".to_string(),
            path: Some(zip_path),
            depth: 0,
            size: 36,
        };

        let bucket_config = BucketConfig {
            alias: "unused".to_string(),
            endpoint: "http://127.0.0.1:1".to_string(),
            bucket: "unused".to_string(),
            access_key_id: "unused".to_string(),
            encryption_key_alias: None,
        };

        let outcome = tokio::runtime::Builder::new_current_thread()
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
                &extracted_bytes,
                &MultiProgress::new(),
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
        }
    }
}
