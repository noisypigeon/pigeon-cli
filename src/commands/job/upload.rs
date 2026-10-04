//! Encrypt-then-upload-to-a-bucket-config primitives shared by every job
//! that can optionally push its local output tree to a remote bucket
//! (originally `email_sync::worker`'s upload phase, ADR-0011/0019/0024/0025;
//! hoisted here once `pull_transform` needed the exact same behavior
//! against a plain local directory instead of a per-identity one, ADR-0074).

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytes::Bytes;
use futures::{StreamExt, stream};
use indicatif::{MultiProgress, ProgressBar};

use crate::commands::job::email_sync::sink;
use crate::commands::keyring::bucket::client;
use crate::commands::keyring::bucket::store::BucketConfig;
use crate::core::crypto::{Aes256GcmSivEncryptor, Encryptor};
use crate::core::data::collect_files;
use crate::core::retry::retry_with_backoff;

const UPLOAD_RETRIES: usize = 3;
const UPLOAD_RETRY_BACKOFF: Duration = Duration::from_secs(2);

/// Floor for `upload_timeout` (ADR-0091 §2) -- comfortably above the real
/// production run's observed upload latency (median 0.17s/file, p99
/// 1.2s/file) but far below the 1800s (30 min) three small files were
/// separately observed to stall for with no timeout at all: a stalled
/// connection should free its concurrency slot and force a fresh-
/// connection retry well before half an hour, not after it.
const UPLOAD_TIMEOUT_FLOOR: Duration = Duration::from_secs(60);

/// Conservative minimum sustained throughput assumed when sizing a timeout
/// for a large file (ADR-0091 §2) -- deliberately pessimistic (far slower
/// than any real S3-compatible endpoint should need) so a genuinely large,
/// genuinely in-progress multipart upload (the real run successfully
/// uploaded a 30.7GB file) is never mistaken for a stall.
const UPLOAD_TIMEOUT_MIN_THROUGHPUT_BYTES_PER_SEC: u64 = 10 * 1024 * 1024;

/// The per-attempt timeout for a `bytes`-sized upload (ADR-0091 §2): a
/// fixed floor (covers the overwhelming majority of real files, which are
/// small) plus an allowance scaled by `bytes` at a conservative assumed
/// minimum throughput -- so a multi-GB file gets proportionally more time
/// before being treated as stalled, without ever waiting anywhere near as
/// long as the 1800s stall this fix targets.
fn upload_timeout(bytes: u64) -> Duration {
    UPLOAD_TIMEOUT_FLOOR + Duration::from_secs(bytes / UPLOAD_TIMEOUT_MIN_THROUGHPUT_BYTES_PER_SEC)
}

/// Runs `fut`, failing with a plain `String` error (the same shape every
/// other failure in this path already uses) if it doesn't finish within
/// `duration` -- so `retry_with_backoff` sees a timeout as just another
/// retryable error, no special-casing needed there or in `client.rs`
/// (ADR-0091 §2).
async fn with_upload_timeout<T>(
    duration: Duration,
    fut: impl std::future::Future<Output = Result<T, String>>,
) -> Result<T, String> {
    match tokio::time::timeout(duration, fut).await {
        Ok(result) => result,
        Err(_) => Err(format!("upload timed out after {duration:?}")),
    }
}

const UPLOADED_FILE_NAME: &str = ".uploaded";

/// One file queued for upload, carrying everything the concurrent upload
/// phase needs without re-deriving it: which caller's `.uploaded` index to
/// commit into, the absolute path to read, and its already-computed S3 key.
/// `label` is purely descriptive (an identity alias, a source bucket alias,
/// ...) -- used only for tracing/log context, never for path computation.
pub(crate) struct UploadTask {
    label: String,
    staging_dir: PathBuf,
    path: PathBuf,
    key: String,
    /// Which job this task belongs to (e.g. "pull-transform", "deduplicate")
    /// -- a static job-type name, not a per-caller label like `label`
    /// above. Used only to tag metrics (ADR-0093) with `pigeon_job`.
    job: &'static str,
}

/// Tracks which output files (by their S3 key, per `upload_key`) have
/// already been confirmed uploaded, so a resumed/re-run upload phase can
/// skip them without a redundant network round-trip. Purely a
/// resumability-speed optimization, not a correctness requirement --
/// `client::upload_if_changed`'s ETag comparison is already idempotent on
/// its own.
pub(crate) struct UploadedIndex {
    uploaded: std::collections::HashSet<String>,
}

impl UploadedIndex {
    fn load(staging_dir: &Path) -> Result<UploadedIndex, String> {
        let path = staging_dir.join(UPLOADED_FILE_NAME);
        let contents = match fs::read_to_string(&path) {
            Ok(contents) => contents,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(err) => return Err(format!("failed to read {}: {err}", path.display())),
        };
        Ok(UploadedIndex {
            uploaded: contents.lines().map(str::to_string).collect(),
        })
    }

    pub(crate) fn contains(&self, key: &str) -> bool {
        self.uploaded.contains(key)
    }

    fn commit(&mut self, staging_dir: &Path, key: &str) -> Result<(), String> {
        use std::io::Write;
        let path = staging_dir.join(UPLOADED_FILE_NAME);
        let mut file = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .map_err(|err| format!("failed to open {}: {err}", path.display()))?;
        writeln!(file, "{key}")
            .map_err(|err| format!("failed to write {}: {err}", path.display()))?;
        self.uploaded.insert(key.to_string());
        Ok(())
    }
}

/// The S3 key for `path` (an absolute path rooted at `key_root`): `key_root`'s
/// relative tree mirrored directly at the bucket root, per ADR-0011. Joined
/// component-wise rather than via `to_string_lossy()` on the whole relative
/// path so the key always uses `/`, regardless of the host platform's path
/// separator. When `encrypt` is true, appends `.enc` so the final key is
/// what actually gets uploaded (ciphertext) and is what
/// `UploadedIndex`/`client::upload_if_changed` key off of -- computed once,
/// here, rather than branched again at upload time (ADR-0025).
pub(crate) fn upload_key(key_root: &Path, path: &Path, encrypt: bool) -> Result<String, String> {
    let relative = path
        .strip_prefix(key_root)
        .map_err(|_| format!("{} is not under {}", path.display(), key_root.display()))?;
    let key = relative
        .components()
        .map(|component| component.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/");
    Ok(if encrypt { format!("{key}.enc") } else { key })
}

/// Outcome of one caller's upload phase.
#[derive(Debug, Default)]
pub(crate) struct UploadSummary {
    pub(crate) uploaded: usize,
    pub(crate) unchanged: usize,
    pub(crate) upload_failed: usize,
}

/// Builds `label`'s not-yet-uploaded file list (per ADR-0019's `.uploaded`
/// tracking) and loads its index, without uploading anything -- kept
/// separate from the upload itself (ADR-0024 §1) so it's unit-testable
/// without any network call, and so every caller's tasks can be gathered
/// into one shared queue before the concurrent upload phase runs. `walk_dir`
/// is the directory whose files become upload candidates; `key_root` is what
/// each candidate's S3 key is computed relative to -- for `email_sync`
/// these differ (an identity's own result subdirectory nested one level
/// under its output root); for a single flat tree like `pull_transform`'s
/// they're the same directory.
pub(crate) fn pending_upload_tasks(
    job: &'static str,
    label: &str,
    staging_dir: &Path,
    walk_dir: &Path,
    key_root: &Path,
    encrypt: bool,
) -> Result<(Vec<UploadTask>, UploadedIndex), String> {
    let uploaded_index = UploadedIndex::load(staging_dir)?;
    let mut tasks = Vec::new();
    for path in collect_files(walk_dir)? {
        let key = upload_key(key_root, &path, encrypt)?;
        if !uploaded_index.contains(&key) {
            tasks.push(UploadTask {
                label: label.to_string(),
                staging_dir: staging_dir.to_path_buf(),
                path,
                key,
                job,
            });
        }
    }
    Ok((tasks, uploaded_index))
}

/// Commits a successful upload into the uploading caller's index, locked
/// only for the duration of this call -- distinct callers never contend on
/// each other's lock, only concurrent uploads for the *same* caller do
/// (ADR-0024 §3).
fn commit_uploaded(
    uploaded_indexes: &HashMap<PathBuf, Arc<Mutex<UploadedIndex>>>,
    task: &UploadTask,
) -> Result<(), String> {
    let index = uploaded_indexes
        .get(&task.staging_dir)
        .expect("every task's staging_dir has a registered index");
    index.lock().unwrap().commit(&task.staging_dir, &task.key)
}

/// Outcome of one file's upload attempt, for `run_upload_phase`'s summary
/// fold.
enum UploadOutcomeKind {
    Uploaded,
    Unchanged,
    Failed,
}

/// Reads and uploads one file, retrying transient failures with backoff
/// (ADR-0024 §6), then commits success into its caller's index and advances
/// the shared progress bar. A file that still fails after exhausting
/// retries is warned about via `multi_progress.println` (load-bearing while
/// upload bars are live, ADR-0024 §4/ADR-0015) and counted as failed --
/// never committed, so it's retried again on the job's next invocation.
#[tracing::instrument(
    skip(task, uploaded_indexes, bucket_config, secret, encryptor, bar, multi_progress),
    fields(identity = %task.label, file = %task.path.display())
)]
async fn upload_one(
    task: UploadTask,
    uploaded_indexes: &HashMap<PathBuf, Arc<Mutex<UploadedIndex>>>,
    bucket_config: &BucketConfig,
    secret: &str,
    encryptor: Option<&Aes256GcmSivEncryptor>,
    bar: &ProgressBar,
    multi_progress: &MultiProgress,
) -> UploadOutcomeKind {
    let bytes_for_log = fs::metadata(&task.path).map(|meta| meta.len()).unwrap_or(0);
    // Metrics only, no log line (ADR-0093) -- "upload started" carried no
    // error/context beyond file/bytes, the same telemetry-not-diagnostic
    // shape as the resource sampler's old log line.
    metrics::counter!(
        "pigeon_upload_attempts_total",
        "pigeon_job" => task.job,
        "instance" => crate::observability::instance(),
        "destination_bucket" => bucket_config.alias.clone(),
    )
    .increment(1);
    let upload_started = std::time::Instant::now();

    let outcome = async {
        // No encryptor: hand the client a bare path (ADR-0089) -- it streams
        // the file straight off disk, so each retry below reopens it fresh
        // instead of holding a full-file buffer across attempts. With an
        // encryptor: ADR-0025's AES-256-GCM-SIV is whole-buffer and only
        // ever sees email-sized files, so read+encrypt once here; a retry
        // then clones the cheap ref-counted `Bytes` handle, not the buffer.
        let body = match encryptor {
            Some(encryptor) => {
                let data = fs::read(&task.path)
                    .map_err(|err| format!("failed to read {}: {err}", task.path.display()))?;
                // Deterministic encryption (ADR-0025): identical plaintext
                // always yields identical ciphertext under the same key, so
                // `upload_if_changed`'s ETag-based dedup below needs no
                // changes.
                let encrypted = encryptor.encrypt(&data)?;
                client::UploadBody::Bytes(Bytes::from(encrypted))
            }
            None => client::UploadBody::Path(task.path.clone()),
        };
        let timeout_duration = upload_timeout(bytes_for_log);
        retry_with_backoff(UPLOAD_RETRIES, UPLOAD_RETRY_BACKOFF, || {
            with_upload_timeout(
                timeout_duration,
                client::upload_if_changed(bucket_config, secret, &task.key, body.clone()),
            )
        })
        .await
    }
    .await;
    metrics::histogram!(
        "pigeon_upload_duration_seconds",
        "pigeon_job" => task.job,
        "instance" => crate::observability::instance(),
        "destination_bucket" => bucket_config.alias.clone(),
    )
    .record(upload_started.elapsed().as_secs_f64());

    bar.inc(1);
    match outcome {
        Ok(client::UploadOutcome::Uploaded) => {
            let _ = commit_uploaded(uploaded_indexes, &task);
            metrics::counter!(
                "pigeon_upload_bytes_total",
                "pigeon_job" => task.job,
                "instance" => crate::observability::instance(),
                "destination_bucket" => bucket_config.alias.clone(),
            )
            .increment(bytes_for_log);
            metrics::counter!(
                "pigeon_upload_outcomes_total",
                "pigeon_job" => task.job,
                "outcome" => "uploaded",
                "instance" => crate::observability::instance(),
                "destination_bucket" => bucket_config.alias.clone(),
            )
            .increment(1);
            UploadOutcomeKind::Uploaded
        }
        Ok(client::UploadOutcome::Unchanged) => {
            let _ = commit_uploaded(uploaded_indexes, &task);
            metrics::counter!(
                "pigeon_upload_outcomes_total",
                "pigeon_job" => task.job,
                "outcome" => "unchanged",
                "instance" => crate::observability::instance(),
                "destination_bucket" => bucket_config.alias.clone(),
            )
            .increment(1);
            UploadOutcomeKind::Unchanged
        }
        Err(err) => {
            tracing::warn!(
                identity = %task.label,
                file = %task.path.display(),
                step = "upload",
                error = %err,
                "upload failed after retries"
            );
            let _ = multi_progress.println(format!(
                "Warning: upload failed for {}: {err}",
                task.path.display()
            ));
            metrics::counter!(
                "pigeon_upload_outcomes_total",
                "pigeon_job" => task.job,
                "outcome" => "failed",
                "instance" => crate::observability::instance(),
                "destination_bucket" => bucket_config.alias.clone(),
            )
            .increment(1);
            UploadOutcomeKind::Failed
        }
    }
}

/// Uploads every task in `tasks` -- spanning every caller's not-yet-uploaded
/// files -- concurrently at `concurrency`, via `stream::buffer_unordered`
/// rather than a manual worker pool: uploads have no per-worker session to
/// reuse (`client::upload_if_changed` already builds a fresh S3 client per
/// call), so there's no connection-affinity reason to prefer a worker-pool
/// shape here. Each task is yielded by `stream::iter` exactly once, so no
/// two concurrently in-flight uploads can ever be for the same file
/// (ADR-0024 §2).
pub(crate) async fn run_upload_phase(
    tasks: Vec<UploadTask>,
    uploaded_indexes: &HashMap<PathBuf, Arc<Mutex<UploadedIndex>>>,
    bucket_config: &BucketConfig,
    secret: &str,
    encryptor: Option<&Aes256GcmSivEncryptor>,
    concurrency: usize,
    multi_progress: &MultiProgress,
) -> UploadSummary {
    if tasks.is_empty() {
        return UploadSummary::default();
    }
    // Every task in one call always shares the same job (ADR-0093) -- this
    // is the single choke point all 5 jobs' upload phases funnel through,
    // so it's the one place that needs to mark the local-work-to-uploading
    // transition, rather than each of their 7 call sites doing it
    // separately.
    crate::observability::metrics::set_macro_phase(tasks[0].job, true);
    let bar = sink::new_progress_bar("upload".to_string(), tasks.len() as u64, multi_progress);

    let summary = stream::iter(tasks)
        .map(|task| {
            upload_one(
                task,
                uploaded_indexes,
                bucket_config,
                secret,
                encryptor,
                &bar,
                multi_progress,
            )
        })
        .buffer_unordered(concurrency.max(1))
        .fold(
            UploadSummary::default(),
            |mut summary, outcome| async move {
                match outcome {
                    UploadOutcomeKind::Uploaded => summary.uploaded += 1,
                    UploadOutcomeKind::Unchanged => summary.unchanged += 1,
                    UploadOutcomeKind::Failed => summary.upload_failed += 1,
                }
                summary
            },
        )
        .await;

    bar.finish();
    summary
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upload_timeout_applies_the_floor_for_small_files() {
        assert_eq!(upload_timeout(0), UPLOAD_TIMEOUT_FLOOR);
        assert_eq!(upload_timeout(1024), UPLOAD_TIMEOUT_FLOOR);
    }

    #[test]
    fn upload_timeout_scales_with_size_for_large_files() {
        let thirty_gb = 30u64 * 1024 * 1024 * 1024;
        let timeout = upload_timeout(thirty_gb);
        assert!(timeout > UPLOAD_TIMEOUT_FLOOR);
        assert!(timeout < Duration::from_secs(6 * 60 * 60));
    }

    #[tokio::test]
    async fn with_upload_timeout_converts_a_stalled_future_into_a_string_err() {
        let never = std::future::pending::<Result<(), String>>();
        let result = with_upload_timeout(Duration::from_millis(10), never).await;
        assert!(result.unwrap_err().contains("timed out"));
    }

    #[tokio::test]
    async fn with_upload_timeout_passes_through_a_fast_result_unchanged() {
        let fast = async { Ok::<_, String>("done") };
        assert_eq!(
            with_upload_timeout(Duration::from_secs(10), fast).await,
            Ok("done")
        );
    }

    #[test]
    fn pending_upload_tasks_with_a_pre_seeded_uploaded_index_matches_the_deduplicate_layout() {
        // `deduplicate`'s own `upload_result` calls `pending_upload_tasks`
        // with `walk_dir == key_root` (both `result_dir`), unlike
        // `email_sync`'s nested-identity-subdirectory layout the other
        // fixtures here use (ADR-0089).
        let staging = tempfile::tempdir().unwrap();
        let result_dir = tempfile::tempdir().unwrap();
        fs::write(result_dir.path().join("a.jpg"), b"a").unwrap();
        fs::write(result_dir.path().join("b.jpg"), b"b").unwrap();

        let key_a = upload_key(result_dir.path(), &result_dir.path().join("a.jpg"), false).unwrap();
        fs::write(
            staging.path().join(UPLOADED_FILE_NAME),
            format!("{key_a}\n"),
        )
        .unwrap();

        let (tasks, index) = pending_upload_tasks(
            "deduplicate",
            "deduplicate-alias",
            staging.path(),
            result_dir.path(),
            result_dir.path(),
            false,
        )
        .unwrap();

        assert_eq!(tasks.len(), 1);
        assert_eq!(
            tasks[0].key,
            upload_key(result_dir.path(), &result_dir.path().join("b.jpg"), false).unwrap()
        );
        assert!(index.contains(&key_a));
    }

    #[test]
    fn pending_upload_tasks_includes_every_file_when_index_is_empty() {
        let staging = tempfile::tempdir().unwrap();
        let output = tempfile::tempdir().unwrap();
        let identity_dir = output.path().join("alias-out");
        fs::create_dir_all(&identity_dir).unwrap();
        fs::write(identity_dir.join("a.md"), b"a").unwrap();
        fs::write(identity_dir.join("b.md"), b"b").unwrap();

        let (tasks, index) = pending_upload_tasks(
            "test-job",
            "alias",
            staging.path(),
            &identity_dir,
            output.path(),
            false,
        )
        .unwrap();

        let mut keys: Vec<String> = tasks.iter().map(|task| task.key.clone()).collect();
        keys.sort();
        assert_eq!(
            keys,
            vec!["alias-out/a.md".to_string(), "alias-out/b.md".to_string()]
        );
        assert!(!index.contains("alias-out/a.md"));
    }

    #[test]
    fn pending_upload_tasks_skips_files_already_in_uploaded_index() {
        let staging = tempfile::tempdir().unwrap();
        let output = tempfile::tempdir().unwrap();
        let identity_dir = output.path().join("alias-out");
        fs::create_dir_all(&identity_dir).unwrap();
        fs::write(identity_dir.join("a.md"), b"a").unwrap();
        fs::write(identity_dir.join("b.md"), b"b").unwrap();

        let key_a = upload_key(output.path(), &identity_dir.join("a.md"), false).unwrap();
        fs::write(
            staging.path().join(UPLOADED_FILE_NAME),
            format!("{key_a}\n"),
        )
        .unwrap();

        let (tasks, index) = pending_upload_tasks(
            "test-job",
            "alias",
            staging.path(),
            &identity_dir,
            output.path(),
            false,
        )
        .unwrap();

        assert_eq!(tasks.len(), 1);
        assert_eq!(
            tasks[0].key,
            upload_key(output.path(), &identity_dir.join("b.md"), false).unwrap()
        );
        assert!(index.contains(&key_a));
    }

    #[test]
    fn upload_key_appends_enc_suffix_when_encryption_enabled() {
        let output = tempfile::tempdir().unwrap();
        let path = output.path().join("alias-out").join("a.md");

        assert_eq!(
            upload_key(output.path(), &path, false).unwrap(),
            "alias-out/a.md"
        );
        assert_eq!(
            upload_key(output.path(), &path, true).unwrap(),
            "alias-out/a.md.enc"
        );
    }

    #[test]
    fn pending_upload_tasks_uses_enc_suffixed_keys_when_encryption_enabled() {
        let staging = tempfile::tempdir().unwrap();
        let output = tempfile::tempdir().unwrap();
        let identity_dir = output.path().join("alias-out");
        fs::create_dir_all(&identity_dir).unwrap();
        fs::write(identity_dir.join("a.md"), b"a").unwrap();

        let (tasks, _index) = pending_upload_tasks(
            "test-job",
            "alias",
            staging.path(),
            &identity_dir,
            output.path(),
            true,
        )
        .unwrap();

        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].key, "alias-out/a.md.enc");
    }
}
