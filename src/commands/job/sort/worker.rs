//! Concurrent download + sequential placement pipeline (ADR-0083 §4/§5).
//! Unlike `pull_transform`/`dedupe`, the task list is static -- no zip
//! expansion means no work is discovered mid-run, so fetching uses a plain
//! `stream::buffer_unordered` (the same shape `commands/job/upload.rs`'s
//! own `run_upload_phase` already uses) instead of a growable queue.
//! Placement never hash-checks anything -- a filename collision is always
//! disambiguated via `unique_path`, on the assumption (ADR-0083) that
//! uniqueness was already established by whatever produced the source
//! bucket's contents.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use futures::StreamExt;
use futures::stream;
use indicatif::{MultiProgress, ProgressBar};

use crate::commands::job::download;
use crate::commands::job::email_sync::sink;
use crate::commands::job::upload::{self, UploadedIndex};
use crate::commands::keyring::bucket::store::BucketConfig;
use crate::core::data::{sanitize_filename, unique_path};

use super::manifest::{self, SortTask, extension_of};

/// A fresh, not-yet-existing path under `dir` named by `counter`
/// (monotonically increasing, shared across concurrently-downloading
/// tasks) plus `extension` -- needed because two concurrent downloads for
/// different source keys could otherwise share a basename and collide in
/// `raw_dir`. Own small copy of the same helper `pull_transform`/`dedupe`
/// both already have (those modules are private, unreachable).
fn next_scratch_path(dir: &Path, counter: &AtomicU64, extension: &str) -> Result<PathBuf, String> {
    fs::create_dir_all(dir).map_err(|err| format!("failed to create {}: {err}", dir.display()))?;
    let name = counter.fetch_add(1, Ordering::SeqCst);
    Ok(dir.join(format!("{name:012}.{extension}")))
}

/// One file successfully downloaded to disk, awaiting placement.
pub(crate) struct DownloadedFile {
    pub original_key: String,
    pub extension: String,
    pub raw_path: PathBuf,
}

#[derive(Debug, Default)]
pub(crate) struct FailureBreakdown {
    pub download: usize,
    pub placement: usize,
}

#[derive(Debug, Default)]
pub(crate) struct SortSummary {
    pub placed: usize,
    pub failed: usize,
    pub failure_breakdown: FailureBreakdown,
    pub uploaded: usize,
    pub unchanged: usize,
    pub upload_failed: usize,
}

/// Downloads `task` to `raw_dir`, recording its extension. `None` means
/// the download failed (logged) -- tallied by the caller, not fatal to
/// the run. `bar` is incremented unconditionally so progress stays
/// accurate regardless of outcome.
async fn download_one(
    bucket_config: &BucketConfig,
    secret: &str,
    task: SortTask,
    raw_dir: &Path,
    counter: &AtomicU64,
    multi_progress: &MultiProgress,
    bar: &ProgressBar,
) -> Option<DownloadedFile> {
    let extension = extension_of(&task.key);
    let result: Result<DownloadedFile, String> = async {
        download::check_disk_space(raw_dir, task.size)?;
        let raw_path = next_scratch_path(raw_dir, counter, &extension)?;
        download::download_with_retry(
            bucket_config,
            secret,
            &task.key,
            task.size,
            &raw_path,
            multi_progress,
        )
        .await?;
        Ok(DownloadedFile {
            original_key: task.key.clone(),
            extension,
            raw_path,
        })
    }
    .await;

    bar.inc(1);
    match result {
        Ok(file) => Some(file),
        Err(err) => {
            tracing::warn!(key = %task.key, step = "download", error = %err, "download failed");
            None
        }
    }
}

/// Places `file` under `result_dir/<extension>/`, always disambiguating a
/// filename collision via `unique_path` -- never a hash check, never a
/// merge (ADR-0083's central design point: uniqueness is assumed already
/// established upstream).
fn place_one(result_dir: &Path, file: &DownloadedFile) -> Result<(), String> {
    let extension_dir = result_dir.join(&file.extension);
    fs::create_dir_all(&extension_dir)
        .map_err(|err| format!("failed to create {}: {err}", extension_dir.display()))?;

    let original_name = Path::new(&file.original_key)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(&file.original_key);
    let final_path = unique_path(&extension_dir.join(sanitize_filename(original_name)));

    fs::rename(&file.raw_path, &final_path).map_err(|err| {
        format!(
            "failed to move {} to {}: {err}",
            file.raw_path.display(),
            final_path.display()
        )
    })
}

/// Runs the full sort pipeline: `tasks` (from `Job::gather`, already
/// filtered against the `.processed` checkpoint) are downloaded
/// concurrently at `concurrency`, placed sequentially (flatten + collision
/// handling), then always uploaded to `remote` -- unencrypted, and
/// unconditionally (unlike every other job, `remote` isn't `Option`).
pub(crate) async fn run_sort_job(
    bucket_config: &BucketConfig,
    secret: &str,
    local_output: &Path,
    tasks: Vec<SortTask>,
    concurrency: usize,
    remote: (&BucketConfig, &str),
) -> Result<SortSummary, String> {
    let staging_dir = local_output.join(".staging");
    let raw_dir = staging_dir.join("raw");
    let result_dir = local_output.join("result");
    fs::create_dir_all(&raw_dir)
        .map_err(|err| format!("failed to create {}: {err}", raw_dir.display()))?;
    let counter = AtomicU64::new(0);

    let multi_progress = MultiProgress::new();
    let total = tasks.len() as u64;
    let bar = sink::new_progress_bar("sort".to_string(), total, &multi_progress);

    let results: Vec<Option<DownloadedFile>> = stream::iter(tasks)
        .map(|task| {
            download_one(
                bucket_config,
                secret,
                task,
                &raw_dir,
                &counter,
                &multi_progress,
                &bar,
            )
        })
        .buffer_unordered(concurrency.max(1))
        .collect()
        .await;
    bar.finish();

    let mut downloaded = Vec::with_capacity(results.len());
    let mut download_failed = 0usize;
    for result in results {
        match result {
            Some(file) => downloaded.push(file),
            None => download_failed += 1,
        }
    }

    downloaded.sort_by(|a, b| a.original_key.cmp(&b.original_key));
    let place_bar = sink::new_progress_bar(
        "place".to_string(),
        downloaded.len() as u64,
        &multi_progress,
    );
    let mut placed = 0usize;
    let mut placement_failed = 0usize;
    for file in &downloaded {
        place_bar.inc(1);
        match place_one(&result_dir, file) {
            Ok(()) => {
                placed += 1;
                manifest::append_checkpoint(&staging_dir, &file.original_key)?;
            }
            Err(err) => {
                tracing::warn!(key = %file.original_key, step = "place", error = %err, "failed to place file");
                placement_failed += 1;
            }
        }
    }
    place_bar.finish();

    let mut summary = SortSummary {
        placed,
        failed: download_failed + placement_failed,
        ..Default::default()
    };
    summary.failure_breakdown.download = download_failed;
    summary.failure_breakdown.placement = placement_failed;

    let (remote_bucket, remote_secret) = remote;
    let (upload_tasks, uploaded_index) = upload::pending_upload_tasks(
        &bucket_config.alias,
        &staging_dir,
        &result_dir,
        &result_dir,
        false,
    )?;
    let mut uploaded_indexes: HashMap<PathBuf, Arc<Mutex<UploadedIndex>>> = HashMap::new();
    uploaded_indexes.insert(staging_dir.clone(), Arc::new(Mutex::new(uploaded_index)));
    let upload_summary = upload::run_upload_phase(
        upload_tasks,
        &uploaded_indexes,
        remote_bucket,
        remote_secret,
        None,
        concurrency,
        &multi_progress,
    )
    .await;
    summary.uploaded = upload_summary.uploaded;
    summary.unchanged = upload_summary.unchanged;
    summary.upload_failed = upload_summary.upload_failed;

    Ok(summary)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn next_scratch_path_produces_unique_paths() {
        let dir = tempfile::tempdir().unwrap();
        let counter = AtomicU64::new(0);
        let a = next_scratch_path(dir.path(), &counter, "jpg").unwrap();
        let b = next_scratch_path(dir.path(), &counter, "jpg").unwrap();
        assert_ne!(a, b);
    }

    fn stage_raw(dir: &Path, name: &str, contents: &[u8]) -> PathBuf {
        let path = dir.join(name);
        fs::write(&path, contents).unwrap();
        path
    }

    #[test]
    fn place_one_places_a_lone_file() {
        let staging = tempfile::tempdir().unwrap();
        let result_dir = tempfile::tempdir().unwrap();

        let file = DownloadedFile {
            original_key: "docs/report.pdf".to_string(),
            extension: "pdf".to_string(),
            raw_path: stage_raw(staging.path(), "000000000000.pdf", b"pdf-bytes"),
        };

        place_one(result_dir.path(), &file).unwrap();

        assert!(result_dir.path().join("pdf/report.pdf").exists());
    }

    #[test]
    fn place_one_disambiguates_a_same_named_collision_without_hashing() {
        let staging = tempfile::tempdir().unwrap();
        let result_dir = tempfile::tempdir().unwrap();

        let first = DownloadedFile {
            original_key: "a/report.pdf".to_string(),
            extension: "pdf".to_string(),
            raw_path: stage_raw(staging.path(), "a.pdf", b"aaa"),
        };
        let second = DownloadedFile {
            original_key: "b/report.pdf".to_string(),
            extension: "pdf".to_string(),
            raw_path: stage_raw(staging.path(), "b.pdf", b"bbb"),
        };

        place_one(result_dir.path(), &first).unwrap();
        place_one(result_dir.path(), &second).unwrap();

        // Both are kept, under disambiguated names -- no hash check ever
        // decides these are "the same file."
        assert!(result_dir.path().join("pdf/report.pdf").exists());
        assert!(result_dir.path().join("pdf/report-2.pdf").exists());
        assert_eq!(
            fs::read(result_dir.path().join("pdf/report.pdf")).unwrap(),
            b"aaa"
        );
        assert_eq!(
            fs::read(result_dir.path().join("pdf/report-2.pdf")).unwrap(),
            b"bbb"
        );
    }
}
