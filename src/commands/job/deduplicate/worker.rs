//! Concurrent download + zip-expansion pipeline (ADR-0082 §4): every item is
//! either a zip (expanded, never itself hashed/placed -- members requeued)
//! or a plain file (hashed via `download::sha256_file` and staged for the
//! sequential dedup+placement pass, `dedup.rs`). No classification beyond
//! "is this a zip" -- no media recoding, no date extraction, unlike
//! `pull-transform`; every file is always processed and every zip is always
//! expanded (ADR-0082 §1).

use std::collections::{HashMap, HashSet, VecDeque};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use indicatif::MultiProgress;
use tracing::Instrument;

use crate::commands::job::download;
use crate::commands::job::email_sync::sink;
use crate::commands::job::pull_transform::archive;
use crate::commands::job::upload::{self, UploadedIndex};
use crate::commands::keyring::bucket::store::BucketConfig;
use crate::core::data::ContentIndex;

use super::dedup::{self, DeduplicateDedup, HashedFile};
use super::manifest::{self, DeduplicateTask, extension_of};

/// A fresh, not-yet-existing path under `dir` named by `counter`
/// (monotonically increasing, shared across concurrent workers) plus
/// `extension` -- own small copy of `pull_transform::worker`'s helper
/// (that module is private, unreachable).
fn next_scratch_path(dir: &Path, counter: &AtomicU64, extension: &str) -> Result<PathBuf, String> {
    fs::create_dir_all(dir).map_err(|err| format!("failed to create {}: {err}", dir.display()))?;
    let name = counter.fetch_add(1, Ordering::SeqCst);
    Ok(dir.join(format!("{name:012}.{extension}")))
}

fn is_zip_key(key: &str) -> bool {
    extension_of(key) == "zip"
}

/// One item on the shared work queue -- a top-level bucket object not yet
/// downloaded (`source_key: Some`, `path: None`), or a file already on disk
/// (`path: Some`) -- true both for a completed top-level download and for a
/// zip member streamed straight to disk during a parent's expansion. Only a
/// `depth == 0` item is ever checkpointed. `root_key` is the top-level
/// task's own `display_key`, unchanged through every descendant -- it never
/// participates in path computation, only in tracking whether *any*
/// descendant of a given root failed or lost data, so that root can be
/// excluded from the checkpoint (ADR-0098).
#[derive(Debug)]
struct QueueItem {
    source_key: Option<String>,
    display_key: String,
    path: Option<PathBuf>,
    depth: u32,
    size: u64,
    root_key: String,
}

#[derive(Debug, Default)]
pub(crate) struct FailureBreakdown {
    pub download: usize,
    pub archive: usize,
    pub hash: usize,
    pub placement: usize,
}

impl FailureBreakdown {
    fn merge(&mut self, other: &FailureBreakdown) {
        self.download += other.download;
        self.archive += other.archive;
        self.hash += other.hash;
        self.placement += other.placement;
    }
}

#[derive(Debug)]
enum FailureCategory {
    Download,
    Archive,
    Hash,
}

#[derive(Debug)]
enum ItemOutcome {
    /// `display_key`/`depth` identify the zip that was expanded (checkpoint
    /// candidate iff `depth == 0`); `members` are queued for the next pass.
    /// `dropped` is how many members this zip lost to the per-archive
    /// extraction-ratio cap (ADR-0098) -- nonzero here means real data was
    /// discarded, so the caller both counts it as a failure and taints this
    /// item's root out of the checkpoint.
    ZipExpanded {
        display_key: String,
        depth: u32,
        members: Vec<QueueItem>,
        dropped: usize,
    },
    Hashed {
        depth: u32,
        file: HashedFile,
    },
    /// A top-level (or, defensively, nested) AppleDouble object (`._*`)
    /// skipped before any download/open attempt (ADR-0107). Never a
    /// failure; `depth` decides whether the caller checkpoints it.
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
    Failed {
        category: FailureCategory,
    },
}

/// Downloads (if not already on disk) then either expands `item` (a zip --
/// its own bytes are discarded, never hashed or placed, per ADR-0082 §4) or
/// hashes it for the placement pass. The expansion/hash itself runs via
/// `tokio::task::spawn_blocking` (ADR-0088) -- both are CPU-bound, so they're
/// handed to tokio's blocking-thread pool rather than occupying one of the
/// runtime's own async worker threads for the whole call. Returns the
/// item's `root_key` alongside the outcome: neither `spawn_blocking` nor the
/// caller's own `tokio::spawn` propagate the ambient `tracing` span on
/// their own, so each blocking closure below explicitly re-enters the span
/// captured just before it was spawned (ADR-0098) -- without this, every
/// warning logged here would be missing the `command`/`instance` fields
/// the rest of `pigeon.jsonl` relies on.
async fn process_item(
    bucket_config: &BucketConfig,
    secret: &str,
    item: QueueItem,
    raw_dir: &Path,
    counter: &Arc<AtomicU64>,
    announce: &download::DownloadAnnounce,
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

    let extension = extension_of(&item.display_key);
    let is_zip = extension == "zip";

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
                "deduplicate",
                "download",
                "ok",
                Some(bucket_config.alias.as_str()),
            );
            raw_path
        }
    };

    if is_zip {
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
                // The zip container itself is never hashed or placed -- only
                // its extracted members are (ADR-0082 §4).
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

    let hash_path = path.clone();
    let span = tracing::Span::current();
    let hash_result =
        tokio::task::spawn_blocking(move || span.in_scope(|| download::sha256_file(&hash_path)))
            .await;
    let outcome = match hash_result {
        Ok(Ok(content_hash)) => ItemOutcome::Hashed {
            depth,
            file: HashedFile {
                original_key: item.display_key,
                scratch_path: path,
                extension,
                content_hash,
            },
        },
        Ok(Err(err)) => {
            tracing::warn!(key = %item.display_key, step = "hash", error = %err, "failed to hash file");
            let _ = fs::remove_file(&path);
            ItemOutcome::Failed {
                category: FailureCategory::Hash,
            }
        }
        Err(err) => {
            tracing::error!(key = %item.display_key, step = "hash", error = %err, "hash task panicked");
            let _ = fs::remove_file(&path);
            ItemOutcome::Failed {
                category: FailureCategory::Hash,
            }
        }
    };
    (root_key, outcome)
}

#[derive(Debug, Default)]
pub(crate) struct DeduplicateSummary {
    pub processed: usize,
    pub failed: usize,
    pub failure_breakdown: FailureBreakdown,
    pub duplicates_skipped: usize,
    /// Zip members dropped by the per-archive extraction-ratio cap
    /// (ADR-0098) -- already folded into `failed`/`failure_breakdown.archive`
    /// too, since dropped data is a real failure, but broken out here so
    /// the wizard can print it as its own distinct, named count.
    pub dropped_members: usize,
    /// Top-level AppleDouble objects (`._*`) skipped before any
    /// download/open attempt (ADR-0107) -- never counted in `failed`.
    pub skipped_apple_double: usize,
    /// Archives that genuinely failed to open (password-protected,
    /// corrupt, etc.), already folded into `failed`/
    /// `failure_breakdown.archive` too, but kept here (and written into
    /// `deduplicate-report.txt`) so they're individually actionable
    /// (ADR-0107).
    pub archive_failures: Vec<archive::ArchiveFailure>,
    pub uploaded: usize,
    pub unchanged: usize,
    pub upload_failed: usize,
}

/// Runs the full deduplicate pipeline: `tasks` (from `Job::gather`, already
/// filtered against the `.processed` checkpoint) are downloaded/expanded/
/// hashed concurrently at `concurrency`, placed sequentially (dedup +
/// report), then uploaded (if `remote` is given) -- always unencrypted.
pub(crate) async fn run_deduplicate_job(
    bucket_config: &BucketConfig,
    secret: &str,
    local_output: &Path,
    tasks: Vec<DeduplicateTask>,
    concurrency: usize,
    upload_concurrency: usize,
    remote: Option<(&BucketConfig, &str)>,
) -> Result<DeduplicateSummary, String> {
    crate::observability::metrics::set_macro_phase("deduplicate", false);
    let staging_dir = local_output.join(".staging");
    let raw_dir = staging_dir.join("raw");
    let result_dir = local_output.join("result");
    fs::create_dir_all(&raw_dir)
        .map_err(|err| format!("failed to create {}: {err}", raw_dir.display()))?;
    let counter = Arc::new(AtomicU64::new(0));

    let multi_progress = MultiProgress::new();
    let total = tasks.len() as u64;
    let _ = multi_progress.println(format!("Downloading and processing {total} object(s)..."));
    tracing::info!(total, "deduplicate: download/expand/hash phase starting");
    let bar = sink::new_progress_bar("deduplicate".to_string(), total, &multi_progress);
    let announce = download::DownloadAnnounce::new(bar.clone());

    let queue: Arc<Mutex<VecDeque<QueueItem>>> = Arc::new(Mutex::new(
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
    let in_flight = Arc::new(AtomicUsize::new(0));

    let hashed_files = Arc::new(Mutex::new(Vec::<HashedFile>::new()));
    let finished_root_keys = Arc::new(Mutex::new(Vec::<String>::new()));
    let failure_breakdown = Arc::new(Mutex::new(FailureBreakdown::default()));
    let dropped_members = Arc::new(AtomicUsize::new(0));
    // Any root whose descendant failed outright, or lost a member to the
    // extraction-ratio cap, is excluded from the checkpoint below -- a root
    // zip was previously checkpointed unconditionally the moment it
    // expanded, regardless of what happened to its members, so a rerun
    // could never retry silently-dropped data (ADR-0098).
    let tainted_roots = Arc::new(Mutex::new(HashSet::<String>::new()));
    // Top-level AppleDouble objects skipped before any download/open
    // attempt -- checkpointed unconditionally, separately from
    // `finished_root_keys` (ADR-0107; see the checkpoint loop below for why
    // these can't just reuse that list's `is_zip_key` retain gate).
    let apple_double_skipped_roots = Arc::new(Mutex::new(Vec::<String>::new()));
    // Archives that genuinely failed to open (password-protected, corrupt,
    // etc.), itemized by key/error so a run's report can point at exactly
    // which ones need a human, instead of only a count (ADR-0107).
    let archive_failures = Arc::new(Mutex::new(Vec::<archive::ArchiveFailure>::new()));
    let command_span = tracing::Span::current();

    let worker_count = concurrency.max(1);
    let mut handles = Vec::with_capacity(worker_count);
    for _ in 0..worker_count {
        let queue = Arc::clone(&queue);
        let in_flight = Arc::clone(&in_flight);
        let hashed_files = Arc::clone(&hashed_files);
        let finished_root_keys = Arc::clone(&finished_root_keys);
        let failure_breakdown = Arc::clone(&failure_breakdown);
        let dropped_members = Arc::clone(&dropped_members);
        let tainted_roots = Arc::clone(&tainted_roots);
        let apple_double_skipped_roots = Arc::clone(&apple_double_skipped_roots);
        let archive_failures = Arc::clone(&archive_failures);
        let counter = Arc::clone(&counter);
        let bucket_config = bucket_config.clone();
        let secret = secret.to_string();
        let raw_dir = raw_dir.clone();
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

                    let (root_key, outcome) =
                        process_item(&bucket_config, &secret, item, &raw_dir, &counter, &announce)
                            .await;

                    match outcome {
                        ItemOutcome::ZipExpanded {
                            display_key,
                            depth,
                            members,
                            dropped,
                        } => {
                            crate::observability::metrics::record_phase(
                                "deduplicate",
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
                                tainted_roots.lock().unwrap().insert(root_key);
                                crate::observability::metrics::record_phase(
                                    "deduplicate",
                                    "archive",
                                    "failed",
                                    Some(bucket_config.alias.as_str()),
                                );
                            }
                        }
                        ItemOutcome::Hashed { depth, file } => {
                            crate::observability::metrics::record_phase(
                                "deduplicate",
                                "hash",
                                "ok",
                                Some(bucket_config.alias.as_str()),
                            );
                            if depth == 0 {
                                finished_root_keys
                                    .lock()
                                    .unwrap()
                                    .push(file.original_key.clone());
                            }
                            hashed_files.lock().unwrap().push(file);
                        }
                        ItemOutcome::SkippedAppleDouble { depth } => {
                            if depth == 0 {
                                apple_double_skipped_roots.lock().unwrap().push(root_key);
                            }
                            // Deliberately no failure_breakdown/metrics bump
                            // -- this is not a failure (ADR-0107's whole
                            // point).
                        }
                        ItemOutcome::ArchiveOpenFailed { key, error } => {
                            tainted_roots.lock().unwrap().insert(root_key);
                            failure_breakdown.lock().unwrap().archive += 1;
                            archive_failures
                                .lock()
                                .unwrap()
                                .push(archive::ArchiveFailure { key, error });
                            crate::observability::metrics::record_phase(
                                "deduplicate",
                                "archive",
                                "failed",
                                Some(bucket_config.alias.as_str()),
                            );
                        }
                        ItemOutcome::Failed { category } => {
                            tainted_roots.lock().unwrap().insert(root_key);
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
                                FailureCategory::Hash => {
                                    breakdown.hash += 1;
                                    "hash"
                                }
                            };
                            crate::observability::metrics::record_phase(
                                "deduplicate",
                                phase,
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
            tracing::error!(error = %err, "deduplicate worker task panicked");
            if first_panic.is_none() {
                first_panic = Some(format!("worker task panicked: {err}"));
            }
        }
    }
    if let Some(err) = first_panic {
        return Err(err);
    }
    bar.finish();

    let files = Arc::try_unwrap(hashed_files)
        .map_err(|_| "internal error: hashed file list still shared".to_string())?
        .into_inner()
        .map_err(|_| "internal error: hashed file list lock poisoned".to_string())?;
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
        hashed = files.len(),
        download_failed = failure_breakdown.download,
        archive_failed = failure_breakdown.archive,
        hash_failed = failure_breakdown.hash,
        dropped_members,
        skipped_apple_double = apple_double_skipped_roots.len(),
        "deduplicate: download/expand/hash phase complete"
    );

    // `dedup_index` (the full hash->path map), `merge_records` (the
    // human-readable report -- can run well into the GB range for a large,
    // duplicate-heavy bucket), and `placed_keys` all live only inside this
    // block, so they're dropped here, before the upload phase runs, instead
    // of surviving in `run_deduplicate_job`'s own scope through the whole upload
    // phase afterward (ADR-0089 -- this is what let a real run's RSS keep
    // climbing well past the fetch+hash phase and eventually get OOM-killed
    // partway through upload).
    let placement_summary = {
        let mut dedup_index = DeduplicateDedup(ContentIndex::load(
            &staging_dir,
            dedup::CONTENT_HASHES_FILE,
        )?);
        let (placement_summary, merge_records, placed_keys) =
            dedup::place_and_report(&result_dir, files, &mut dedup_index, &multi_progress);
        dedup::write_report(local_output, &merge_records, &archive_failures)?;

        let placed_keys: std::collections::HashSet<String> = placed_keys.into_iter().collect();
        finished_root_keys.retain(|key| {
            !tainted_roots.contains(key) && (placed_keys.contains(key) || is_zip_key(key))
        });
        for key in &finished_root_keys {
            manifest::append_checkpoint(&staging_dir, key)?;
        }
        // Checkpointed unconditionally and separately from
        // `finished_root_keys` above -- an AppleDouble object has no
        // members, so it can never satisfy that list's `placed_keys`/
        // `is_zip_key` retain gate, and it can never be tainted either
        // (ADR-0107).
        for key in &apple_double_skipped_roots {
            manifest::append_checkpoint(&staging_dir, key)?;
        }
        placement_summary
    };

    tracing::info!(
        placed = placement_summary.placed,
        duplicates_skipped = placement_summary.duplicates_skipped,
        placement_failed = placement_summary.failed,
        "deduplicate: placement phase complete"
    );

    let mut summary = DeduplicateSummary {
        processed: placement_summary.placed,
        failed: failure_breakdown.download
            + failure_breakdown.archive
            + failure_breakdown.hash
            + placement_summary.failed,
        duplicates_skipped: placement_summary.duplicates_skipped,
        dropped_members,
        skipped_apple_double: apple_double_skipped_roots.len(),
        archive_failures,
        ..Default::default()
    };
    summary.failure_breakdown.merge(&failure_breakdown);
    // `place_and_report`'s placement failures were previously logged but
    // never counted anywhere at all (ADR-0093 fixes this as a side effect
    // of adding the live per-phase metric at that same call site) --
    // `pull_transform`'s equivalent placement pass already folds its own
    // `PlacementSummary.failed` into `failure_breakdown.placement` the same
    // way.
    summary.failure_breakdown.placement = placement_summary.failed;

    if let Some((remote_bucket, remote_secret)) = remote {
        tracing::info!(bucket = %remote_bucket.alias, "deduplicate: upload phase starting");
        let upload_summary = upload_result(
            &remote_bucket.alias,
            local_output,
            (remote_bucket, remote_secret),
            upload_concurrency,
            &multi_progress,
        )
        .await?;
        log_upload_phase_complete(&upload_summary);
        summary.uploaded = upload_summary.uploaded;
        summary.unchanged = upload_summary.unchanged;
        summary.upload_failed = upload_summary.upload_failed;
    }

    Ok(summary)
}

/// Uploads `local_output/result/` to `remote`, resuming via the existing
/// `.staging/.uploaded` index (ADR-0019/ADR-0024) -- the shared upload tail
/// both `run_deduplicate_job` and `run_upload_only` call, so there's one code
/// path and one resume mechanism between a fresh run and a resumed
/// `--upload-only` one (ADR-0089). `label` is purely descriptive (tracing/
/// log context): both callers pass `remote`'s own alias, since `remote` is
/// always the actual upload destination (ADR-0098 -- `run_deduplicate_job`
/// previously passed the source bucket's alias here, mislabeling every
/// upload-phase log line with the wrong bucket).
/// Logs the upload phase's completion (ADR-0104), matching the shape of
/// `run_deduplicate_job`'s other three phase-boundary lines (ADR-0099) --
/// extracted into its own function so it's unit-testable without standing
/// up a full `run_deduplicate_job`/mock-bucket fixture.
fn log_upload_phase_complete(summary: &upload::UploadSummary) {
    tracing::info!(
        uploaded = summary.uploaded,
        unchanged = summary.unchanged,
        upload_failed = summary.upload_failed,
        "deduplicate: upload phase complete"
    );
}

async fn upload_result(
    label: &str,
    local_output: &Path,
    remote: (&BucketConfig, &str),
    upload_concurrency: usize,
    multi_progress: &MultiProgress,
) -> Result<upload::UploadSummary, String> {
    let staging_dir = local_output.join(".staging");
    let result_dir = local_output.join("result");
    let (remote_bucket, remote_secret) = remote;

    let (upload_tasks, uploaded_index) = upload::pending_upload_tasks(
        "deduplicate",
        label,
        &staging_dir,
        &result_dir,
        &result_dir,
        false,
    )?;
    let mut uploaded_indexes: HashMap<PathBuf, Arc<Mutex<UploadedIndex>>> = HashMap::new();
    uploaded_indexes.insert(staging_dir.clone(), Arc::new(Mutex::new(uploaded_index)));
    Ok(upload::run_upload_phase(
        upload_tasks,
        &uploaded_indexes,
        remote_bucket,
        remote_secret,
        None,
        upload_concurrency,
        multi_progress,
    )
    .await)
}

/// Resumes uploading an already-completed local deduplicate run, skipping the
/// bucket listing/download/hash/placement phases entirely (ADR-0089's
/// `--upload-only`). Reuses `upload_result`, the same helper
/// `run_deduplicate_job`'s own upload tail calls.
pub(crate) async fn run_upload_only(
    local_output: &Path,
    remote: (&BucketConfig, &str),
    upload_concurrency: usize,
) -> Result<upload::UploadSummary, String> {
    let multi_progress = MultiProgress::new();
    let label = remote.0.alias.clone();
    upload_result(
        &label,
        local_output,
        remote,
        upload_concurrency,
        &multi_progress,
    )
    .await
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use super::*;

    #[test]
    fn is_zip_key_detects_zip_extension_case_insensitively() {
        assert!(is_zip_key("archive.zip"));
        assert!(is_zip_key("nested/ARCHIVE.ZIP"));
        assert!(!is_zip_key("photo.jpg"));
    }

    #[test]
    fn next_scratch_path_produces_unique_paths() {
        let dir = tempfile::tempdir().unwrap();
        let counter = AtomicU64::new(0);
        let a = next_scratch_path(dir.path(), &counter, "jpg").unwrap();
        let b = next_scratch_path(dir.path(), &counter, "jpg").unwrap();
        assert_ne!(a, b);
    }

    fn unreachable_bucket_config() -> BucketConfig {
        BucketConfig {
            alias: "unused".to_string(),
            endpoint: "http://127.0.0.1:1".to_string(),
            bucket: "unused".to_string(),
            access_key_id: "unused".to_string(),
            encryption_key_alias: None,
        }
    }

    #[tokio::test]
    async fn process_item_skips_a_top_level_apple_double_zip_without_downloading() {
        let dir = tempfile::tempdir().unwrap();
        let raw_dir = dir.path().join("raw");
        let counter = Arc::new(AtomicU64::new(0));
        let bucket_config = unreachable_bucket_config();

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
            &counter,
            &download::DownloadAnnounce::new(indicatif::ProgressBar::hidden()),
        )
        .await;

        assert!(matches!(
            outcome,
            ItemOutcome::SkippedAppleDouble { depth: 0 }
        ));
        assert!(!raw_dir.exists() || fs::read_dir(&raw_dir).unwrap().next().is_none());
    }

    #[tokio::test]
    async fn process_item_still_expands_a_real_zip_with_a_similar_name() {
        let dir = tempfile::tempdir().unwrap();
        let raw_dir = dir.path().join("raw");
        fs::create_dir_all(&raw_dir).unwrap();
        let counter = Arc::new(AtomicU64::new(0));
        let bucket_config = unreachable_bucket_config();

        let zip_path = raw_dir.join("something.zip");
        let file = fs::File::create(&zip_path).unwrap();
        let mut writer = zip::ZipWriter::new(file);
        writer
            .start_file(
                "hello.txt",
                zip::write::SimpleFileOptions::default()
                    .compression_method(zip::CompressionMethod::Stored),
            )
            .unwrap();
        writer.write_all(b"hello").unwrap();
        writer.finish().unwrap();

        let item = QueueItem {
            source_key: None,
            display_key: "something.zip".to_string(),
            path: Some(zip_path),
            depth: 0,
            size: 0,
            root_key: "something.zip".to_string(),
        };

        let (_root_key, outcome) = process_item(
            &bucket_config,
            "unused-secret",
            item,
            &raw_dir,
            &counter,
            &download::DownloadAnnounce::new(indicatif::ProgressBar::hidden()),
        )
        .await;

        match outcome {
            ItemOutcome::ZipExpanded { members, .. } => {
                assert_eq!(members.len(), 1);
                assert!(members[0].display_key.ends_with("!hello.txt"));
            }
            other => panic!("expected ZipExpanded, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn process_item_captures_the_key_and_error_for_a_genuinely_failed_archive() {
        let dir = tempfile::tempdir().unwrap();
        let raw_dir = dir.path().join("raw");
        fs::create_dir_all(&raw_dir).unwrap();
        let counter = Arc::new(AtomicU64::new(0));
        let bucket_config = unreachable_bucket_config();

        let zip_path = raw_dir.join("some.zip");
        fs::write(&zip_path, b"not really a zip, just opaque bytes").unwrap();

        let item = QueueItem {
            source_key: None,
            display_key: "some.zip".to_string(),
            path: Some(zip_path),
            depth: 0,
            size: 0,
            root_key: "some.zip".to_string(),
        };

        let (_root_key, outcome) = process_item(
            &bucket_config,
            "unused-secret",
            item,
            &raw_dir,
            &counter,
            &download::DownloadAnnounce::new(indicatif::ProgressBar::hidden()),
        )
        .await;

        match outcome {
            ItemOutcome::ArchiveOpenFailed { key, .. } => assert_eq!(key, "some.zip"),
            other => panic!("expected ArchiveOpenFailed, got {other:?}"),
        }
    }

    /// Mirrors the `CapturedEvents`/`CaptureLayer` pattern already
    /// established in `email_sync/transform.rs`'s test module (ADR-0080);
    /// duplicated here per this codebase's "duplicate until the third
    /// consumer" precedent.
    #[derive(Clone, Default)]
    struct CapturedEvents(
        std::sync::Arc<std::sync::Mutex<Vec<std::collections::HashMap<String, String>>>>,
    );

    struct CaptureLayer(CapturedEvents);

    impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for CaptureLayer {
        fn on_event(
            &self,
            event: &tracing::Event<'_>,
            _ctx: tracing_subscriber::layer::Context<'_, S>,
        ) {
            struct Visitor(std::collections::HashMap<String, String>);
            impl tracing::field::Visit for Visitor {
                fn record_debug(
                    &mut self,
                    field: &tracing::field::Field,
                    value: &dyn std::fmt::Debug,
                ) {
                    self.0
                        .insert(field.name().to_string(), format!("{value:?}"));
                }
                fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
                    self.0.insert(field.name().to_string(), value.to_string());
                }
            }
            let mut visitor = Visitor(std::collections::HashMap::new());
            event.record(&mut visitor);
            self.0.0.lock().unwrap().push(visitor.0);
        }
    }

    fn capture_events(run: impl FnOnce()) -> Vec<std::collections::HashMap<String, String>> {
        use tracing_subscriber::layer::SubscriberExt as _;

        let events = CapturedEvents::default();
        let subscriber = tracing_subscriber::registry().with(CaptureLayer(events.clone()));
        tracing::subscriber::with_default(subscriber, run);
        events.0.lock().unwrap().clone()
    }

    #[test]
    fn log_upload_phase_complete_logs_the_upload_summary_counts() {
        let summary = upload::UploadSummary {
            uploaded: 3,
            unchanged: 2,
            upload_failed: 1,
        };
        let events = capture_events(|| log_upload_phase_complete(&summary));
        assert_eq!(events.len(), 1);
        let fields = &events[0];
        assert_eq!(
            fields.get("message").map(String::as_str),
            Some("deduplicate: upload phase complete")
        );
        assert_eq!(fields.get("uploaded").map(String::as_str), Some("3"));
        assert_eq!(fields.get("unchanged").map(String::as_str), Some("2"));
        assert_eq!(fields.get("upload_failed").map(String::as_str), Some("1"));
    }
}
