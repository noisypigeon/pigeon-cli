//! Concurrent download + zip-expansion pipeline (ADR-0082 §4): every item is
//! either a zip (expanded, never itself hashed/placed -- members requeued)
//! or a plain file (hashed via `download::sha256_file` and staged for the
//! sequential dedup+placement pass, `dedup.rs`). No classification beyond
//! "is this a zip" -- no media recoding, no date extraction, unlike
//! `pull-transform`; every file is always processed and every zip is always
//! expanded (ADR-0082 §1).

use std::collections::{HashMap, VecDeque};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use indicatif::MultiProgress;

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
/// `depth == 0` item is ever checkpointed.
struct QueueItem {
    source_key: Option<String>,
    display_key: String,
    path: Option<PathBuf>,
    depth: u32,
    size: u64,
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

enum FailureCategory {
    Download,
    Archive,
    Hash,
}

enum ItemOutcome {
    /// `display_key`/`depth` identify the zip that was expanded (checkpoint
    /// candidate iff `depth == 0`); `members` are queued for the next pass.
    ZipExpanded {
        display_key: String,
        depth: u32,
        members: Vec<QueueItem>,
    },
    Hashed {
        depth: u32,
        file: HashedFile,
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
/// runtime's own async worker threads for the whole call.
async fn process_item(
    bucket_config: &BucketConfig,
    secret: &str,
    item: QueueItem,
    raw_dir: &Path,
    counter: &Arc<AtomicU64>,
    extracted_bytes: &Arc<AtomicU64>,
    multi_progress: &MultiProgress,
) -> ItemOutcome {
    let depth = item.depth;
    let extension = extension_of(&item.display_key);
    let is_zip = extension == "zip";

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
        let expand_path = path.clone();
        let expand_raw_dir = raw_dir.to_path_buf();
        let expand_counter = Arc::clone(counter);
        let expand_extracted_bytes = Arc::clone(extracted_bytes);
        let expand_result = tokio::task::spawn_blocking(move || {
            archive::expand_to_dir(
                &expand_path,
                &expand_raw_dir,
                &expand_counter,
                &expand_extracted_bytes,
            )
        })
        .await;
        return match expand_result {
            Ok(Ok(raw_members)) => {
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
                    })
                    .collect();
                ItemOutcome::ZipExpanded {
                    display_key: item.display_key,
                    depth,
                    members,
                }
            }
            Ok(Err(err)) => {
                tracing::warn!(key = %item.display_key, step = "archive", error = %err, "failed to open zip archive");
                let _ = fs::remove_file(&path);
                ItemOutcome::Failed {
                    category: FailureCategory::Archive,
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
    }

    let hash_path = path.clone();
    let hash_result = tokio::task::spawn_blocking(move || download::sha256_file(&hash_path)).await;
    match hash_result {
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
    }
}

#[derive(Debug, Default)]
pub(crate) struct DeduplicateSummary {
    pub processed: usize,
    pub failed: usize,
    pub failure_breakdown: FailureBreakdown,
    pub duplicates_skipped: usize,
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
    let extracted_bytes = Arc::new(AtomicU64::new(0));

    let multi_progress = MultiProgress::new();
    let total = tasks.len() as u64;
    let _ = multi_progress.println(format!("Downloading and processing {total} object(s)..."));
    let bar = sink::new_progress_bar("deduplicate".to_string(), total, &multi_progress);

    let queue: Arc<Mutex<VecDeque<QueueItem>>> = Arc::new(Mutex::new(
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
    let in_flight = Arc::new(AtomicUsize::new(0));

    let hashed_files = Arc::new(Mutex::new(Vec::<HashedFile>::new()));
    let finished_root_keys = Arc::new(Mutex::new(Vec::<String>::new()));
    let failure_breakdown = Arc::new(Mutex::new(FailureBreakdown::default()));

    let worker_count = concurrency.max(1);
    let mut handles = Vec::with_capacity(worker_count);
    for _ in 0..worker_count {
        let queue = Arc::clone(&queue);
        let in_flight = Arc::clone(&in_flight);
        let hashed_files = Arc::clone(&hashed_files);
        let finished_root_keys = Arc::clone(&finished_root_keys);
        let failure_breakdown = Arc::clone(&failure_breakdown);
        let counter = Arc::clone(&counter);
        let extracted_bytes = Arc::clone(&extracted_bytes);
        let bucket_config = bucket_config.clone();
        let secret = secret.to_string();
        let raw_dir = raw_dir.clone();
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
                    &counter,
                    &extracted_bytes,
                    &multi_progress,
                )
                .await;

                match outcome {
                    ItemOutcome::ZipExpanded {
                        display_key,
                        depth,
                        members,
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
                    ItemOutcome::Failed { category } => {
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
        }));
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
        dedup::write_report(local_output, &merge_records)?;

        let placed_keys: std::collections::HashSet<String> = placed_keys.into_iter().collect();
        finished_root_keys.retain(|key| placed_keys.contains(key) || is_zip_key(key));
        for key in &finished_root_keys {
            manifest::append_checkpoint(&staging_dir, key)?;
        }
        placement_summary
    };

    let mut summary = DeduplicateSummary {
        processed: placement_summary.placed,
        failed: failure_breakdown.download
            + failure_breakdown.archive
            + failure_breakdown.hash
            + placement_summary.failed,
        duplicates_skipped: placement_summary.duplicates_skipped,
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
        let upload_summary = upload_result(
            &bucket_config.alias,
            local_output,
            (remote_bucket, remote_secret),
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

/// Uploads `local_output/result/` to `remote`, resuming via the existing
/// `.staging/.uploaded` index (ADR-0019/ADR-0024) -- the shared upload tail
/// both `run_deduplicate_job` and `run_upload_only` call, so there's one code
/// path and one resume mechanism between a fresh run and a resumed
/// `--upload-only` one (ADR-0089). `label` is purely descriptive
/// (tracing/log context): `run_deduplicate_job` passes the source bucket's
/// alias (its historical behavior); `run_upload_only`, which never touches
/// a source bucket at all, passes the remote's own alias instead.
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
}
