//! Concurrent download + placement pipeline (ADR-0096 §2): every task was
//! already classified `Valuable` during `gather_pending`, so there's no
//! per-item classification, no hashing, no zip-expansion, and nothing
//! CPU-bound here -- just a plain streamed download (reusing
//! `commands/job/download.rs`) followed by a move into
//! `result/<extension>/`. A fixed task list with no dynamic requeueing
//! (unlike `deduplicate`'s zip-expansion-driven queue), so a plain
//! `stream::buffer_unordered` concurrent pass is enough -- no worker-pool
//! machinery needed.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use futures::{StreamExt, stream};
use indicatif::MultiProgress;

use crate::commands::job::download;
use crate::commands::job::email_sync::sink;
use crate::commands::job::upload::{self, UploadedIndex};
use crate::commands::keyring::bucket::store::BucketConfig;
use crate::core::data::{extension_of, sanitize_filename, unique_path};

use super::classify::ContentValue;
use super::manifest::{self, ReducePlan, ReduceTask};

#[derive(Debug, Default)]
pub(crate) struct FailureBreakdown {
    pub download: usize,
    pub placement: usize,
}

enum FailureCategory {
    Download,
    Placement,
}

enum ItemOutcome {
    Placed { key: String },
    Failed { category: FailureCategory },
}

/// Downloads `task` straight to `raw_dir`, then moves it into
/// `result_dir/<extension>/<sanitized-unique-name>` -- no dedup/content-hash
/// check (`reduce` filters, it doesn't dedupe; its input is already
/// unique).
async fn process_item(
    bucket_config: &BucketConfig,
    secret: &str,
    task: ReduceTask,
    raw_dir: &Path,
    result_dir: &Path,
    multi_progress: &MultiProgress,
) -> ItemOutcome {
    let extension = extension_of(&task.key);
    if let Err(err) = download::check_disk_space(raw_dir, task.size) {
        tracing::warn!(key = %task.key, step = "download", error = %err, "not enough disk space");
        crate::observability::metrics::record_phase("reduce", "download", "failed");
        return ItemOutcome::Failed {
            category: FailureCategory::Download,
        };
    }
    let raw_path = raw_dir.join(sanitize_filename(&task.key.replace('/', "_")));
    if let Err(err) = download::download_with_retry(
        bucket_config,
        secret,
        &task.key,
        task.size,
        &raw_path,
        multi_progress,
    )
    .await
    {
        tracing::warn!(key = %task.key, step = "download", error = %err, "download failed");
        let _ = fs::remove_file(&raw_path);
        crate::observability::metrics::record_phase("reduce", "download", "failed");
        return ItemOutcome::Failed {
            category: FailureCategory::Download,
        };
    }
    crate::observability::metrics::record_phase("reduce", "download", "ok");

    let extension_dir = result_dir.join(&extension);
    if let Err(err) = fs::create_dir_all(&extension_dir) {
        tracing::warn!(key = %task.key, step = "placement", error = %err, "failed to create extension dir");
        let _ = fs::remove_file(&raw_path);
        crate::observability::metrics::record_phase("reduce", "placement", "failed");
        return ItemOutcome::Failed {
            category: FailureCategory::Placement,
        };
    }
    let original_name = Path::new(&task.key)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(&task.key);
    let final_path = unique_path(&extension_dir.join(sanitize_filename(original_name)));
    if let Err(err) = fs::rename(&raw_path, &final_path) {
        tracing::warn!(key = %task.key, step = "placement", error = %err, "failed to place file");
        let _ = fs::remove_file(&raw_path);
        crate::observability::metrics::record_phase("reduce", "placement", "failed");
        return ItemOutcome::Failed {
            category: FailureCategory::Placement,
        };
    }
    crate::observability::metrics::record_phase("reduce", "placement", "ok");
    ItemOutcome::Placed { key: task.key }
}

#[derive(Debug, Default)]
pub(crate) struct ReduceSummary {
    pub forwarded: usize,
    pub skipped_low_value: usize,
    pub failed: usize,
    pub failure_breakdown: FailureBreakdown,
    pub uploaded: usize,
    pub unchanged: usize,
    pub upload_failed: usize,
}

/// Runs the full reduce pipeline: `plan.tasks` (from `Job::gather`, already
/// filtered down to `Valuable`-classified keys only, and already excluding
/// the `.processed` checkpoint) are downloaded/placed concurrently at
/// `concurrency`, then uploaded (always, `remote` is mandatory for this
/// job) -- always unencrypted. `skipped_low_value` is derived from
/// `plan.extension_summary`, not recomputed from scratch.
pub(crate) async fn run_reduce_job(
    bucket_config: &BucketConfig,
    secret: &str,
    local_output: &Path,
    plan: ReducePlan,
    concurrency: usize,
    upload_concurrency: usize,
    remote: (&BucketConfig, &str),
) -> Result<ReduceSummary, String> {
    let skipped_low_value: usize = plan
        .extension_summary
        .iter()
        .filter(|summary| summary.value == ContentValue::Reproducible)
        .map(|summary| summary.count)
        .sum();
    let tasks = plan.tasks;

    crate::observability::metrics::set_macro_phase("reduce", false);
    let staging_dir = local_output.join(".staging");
    let raw_dir = staging_dir.join("raw");
    let result_dir = local_output.join("result");
    fs::create_dir_all(&raw_dir)
        .map_err(|err| format!("failed to create {}: {err}", raw_dir.display()))?;

    let multi_progress = MultiProgress::new();
    let total = tasks.len() as u64;
    let _ = multi_progress.println(format!("Downloading and placing {total} object(s)..."));
    let bar = sink::new_progress_bar("reduce".to_string(), total, &multi_progress);

    let mut failure_breakdown = FailureBreakdown::default();
    let mut forwarded = 0usize;
    let mut finished_keys = Vec::with_capacity(tasks.len());

    let results: Vec<ItemOutcome> = stream::iter(tasks)
        .map(|task| {
            process_item(
                bucket_config,
                secret,
                task,
                &raw_dir,
                &result_dir,
                &multi_progress,
            )
        })
        .buffer_unordered(concurrency.max(1))
        .collect()
        .await;

    for outcome in results {
        bar.inc(1);
        match outcome {
            ItemOutcome::Placed { key } => {
                forwarded += 1;
                finished_keys.push(key);
            }
            ItemOutcome::Failed { category } => match category {
                FailureCategory::Download => failure_breakdown.download += 1,
                FailureCategory::Placement => failure_breakdown.placement += 1,
            },
        }
    }
    bar.finish();

    for key in &finished_keys {
        manifest::append_checkpoint(&staging_dir, key)?;
    }

    let mut summary = ReduceSummary {
        forwarded,
        skipped_low_value,
        failed: failure_breakdown.download + failure_breakdown.placement,
        failure_breakdown,
        ..Default::default()
    };

    let upload_summary = upload_result(
        &bucket_config.alias,
        local_output,
        remote,
        upload_concurrency,
        &multi_progress,
    )
    .await?;
    summary.uploaded = upload_summary.uploaded;
    summary.unchanged = upload_summary.unchanged;
    summary.upload_failed = upload_summary.upload_failed;

    Ok(summary)
}

/// Uploads `local_output/result/` to `remote`, resuming via the existing
/// `.staging/.uploaded` index (ADR-0019/ADR-0024) -- the shared upload tail
/// both `run_reduce_job` and `run_upload_only` call, mirroring
/// `deduplicate::worker`'s shape exactly.
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
        "reduce",
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

/// Resumes uploading an already-completed local reduce run, skipping the
/// bucket listing/download/placement phases entirely (ADR-0090's
/// `--upload-only` shape). Reuses `upload_result`, the same helper
/// `run_reduce_job`'s own upload tail calls.
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
