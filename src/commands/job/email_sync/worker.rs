use std::collections::{HashMap, HashSet, VecDeque};
use std::fs;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::{StreamExt, stream};
use indicatif::{MultiProgress, ProgressBar};

use crate::commands::keyring::bucket::client;
use crate::commands::keyring::bucket::store::BucketConfig;
use crate::commands::keyring::email::identity;
use crate::commands::keyring::email::imap_client::{self, ImapSession};
use crate::core::crypto::{Aes256GcmSivEncryptor, Encryptor};
use crate::core::data::{ContentIndex, Transform, collect_files};

use super::dedup::{self, EmailDedup};
use super::manifest::{self, Batch, CheckpointEntry};
use super::sink;
use super::transform::{self, EmailTransform};
use super::{IdentityContext, PendingMailbox};

const CONNECT_RETRIES: usize = 3;
const RETRY_BACKOFF: Duration = Duration::from_secs(5);

/// Retries `f` up to `attempts` times, sleeping `backoff * attempt_number`
/// between tries (linear: `backoff`, `2*backoff`, ...) before giving up --
/// absorbs a transient provider-side throttle (e.g. a burst of
/// `concurrency` workers all connecting within the same instant at job
/// start) instead of failing on the first timeout. Generic over `f` so it's
/// testable without any real I/O (see the unit tests below).
pub(crate) async fn retry_with_backoff<T, F, Fut>(
    attempts: usize,
    backoff: Duration,
    mut f: F,
) -> Result<T, String>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T, String>>,
{
    let mut last_err = None;
    for attempt in 0..attempts.max(1) {
        if attempt > 0 {
            tokio::time::sleep(backoff * attempt as u32).await;
        }
        match f().await {
            Ok(value) => return Ok(value),
            Err(err) => last_err = Some(err),
        }
    }
    Err(last_err.unwrap_or_else(|| "retry_with_backoff called with zero attempts".to_string()))
}

/// Connects and logs in to `ctx`'s identity, retrying on failure per
/// `retry_with_backoff` (ADR-0021 §6 addendum) -- used by `gather_pending`'s
/// one-shot per-identity connection and by each persistent worker's initial
/// connect/reconnect (`run_worker`, below).
pub(crate) async fn connect_with_retry(ctx: &IdentityContext) -> Result<ImapSession, String> {
    retry_with_backoff(CONNECT_RETRIES, RETRY_BACKOFF, || {
        imap_client::connect_and_login(
            &ctx.identity.host,
            ctx.identity.port,
            &ctx.identity.email,
            &ctx.secret,
            ctx.identity.provider.accepts_invalid_certs(),
        )
    })
    .await
}

/// Outcome of one worker's processing, for the job-level summary.
#[derive(Default)]
struct BatchOutcome {
    synced: usize,
    failed: usize,
}

/// A worker's currently-open IMAP session, if any -- tracks which identity
/// and mailbox it's scoped to, so `run_worker` can tell whether its next
/// batch needs a full reconnect (different identity -> different
/// credentials) or just a re-`EXAMINE` (same identity, different mailbox --
/// cheap, no new TCP/TLS/LOGIN) before it can be processed.
struct WorkerConnection {
    identity_index: usize,
    mailbox: String,
    session: ImapSession,
}

/// Pulls `(identity_index, Batch)` pairs from `queue` until it's drained,
/// holding one IMAP session per identity for as long as consecutive batches
/// it pulls belong to that identity (ADR-0021 §6 addendum) -- the direct fix
/// for the connection-churn bug: a job with `concurrency` workers now opens
/// on the order of `concurrency` connections total over its whole run, not
/// one per batch. A connect/re-`EXAMINE` failure (even after
/// `connect_with_retry`'s retries) drops the current session and counts
/// that batch's UIDs as failed, then moves on to the next queued batch --
/// which may belong to a different, unaffected identity -- rather than
/// aborting the worker outright.
async fn run_worker(
    queue: Arc<Mutex<VecDeque<(usize, Batch)>>>,
    identities: Arc<Vec<IdentityContext>>,
    multi_progress: MultiProgress,
) -> BatchOutcome {
    let mut connection: Option<WorkerConnection> = None;
    let mut outcome = BatchOutcome::default();

    loop {
        let next = { queue.lock().unwrap().pop_front() };
        let Some((identity_index, batch)) = next else {
            break;
        };
        let ctx = &identities[identity_index];

        if connection
            .as_ref()
            .is_none_or(|conn| conn.identity_index != identity_index)
        {
            if let Some(mut conn) = connection.take() {
                let _ = conn.session.logout().await;
            }
            match connect_with_retry(ctx).await {
                Ok(session) => {
                    connection = Some(WorkerConnection {
                        identity_index,
                        mailbox: String::new(),
                        session,
                    });
                }
                Err(err) => {
                    let _ = multi_progress.println(format!("Error: {err}"));
                    outcome.failed += batch.uids.len();
                    continue;
                }
            }
        }

        let conn = connection.as_mut().unwrap();
        if conn.mailbox != batch.mailbox {
            match conn.session.examine(&batch.mailbox).await {
                Ok(_) => conn.mailbox = batch.mailbox.clone(),
                Err(err) => {
                    let _ = multi_progress.println(format!(
                        "Error: failed to open '{}' read-only: {err}",
                        batch.mailbox
                    ));
                    outcome.failed += batch.uids.len();
                    connection = None;
                    continue;
                }
            }
        }

        let conn = connection.as_mut().unwrap();
        match process_batch_on_session(ctx, &batch, &mut conn.session, &multi_progress).await {
            Ok(batch_outcome) => {
                outcome.synced += batch_outcome.synced;
                outcome.failed += batch_outcome.failed;
            }
            Err(err) => {
                let _ = multi_progress.println(format!("Error: {err}"));
                outcome.failed += batch.uids.len();
                connection = None;
            }
        }
    }

    if let Some(mut conn) = connection {
        let _ = conn.session.logout().await;
    }

    outcome
}

/// Fetches, transforms, and verifies every UID in `batch` on an
/// already-connected, already-`EXAMINE`d `session` (owned by the calling
/// `run_worker`, reused across every batch it processes for the same
/// identity/mailbox), appending a checkpoint entry for each UID that
/// verifies and deleting its `.eml` -- no lock of any kind, since a batch's
/// UIDs are staged under a UID-keyed tree exclusive to this worker
/// (ADR-0021 §7/§10, addendum).
async fn process_batch_on_session(
    ctx: &IdentityContext,
    batch: &Batch,
    session: &mut ImapSession,
    multi_progress: &MultiProgress,
) -> Result<BatchOutcome, String> {
    let mailbox_dir = ctx.staging_dir.join(&batch.mailbox_relpath);
    fs::create_dir_all(&mailbox_dir)
        .map_err(|err| format!("failed to create {}: {err}", mailbox_dir.display()))?;

    sink::fetch_uids(
        session,
        &batch.mailbox,
        &mailbox_dir,
        &batch.uids,
        multi_progress,
    )
    .await?;

    let transformer = EmailTransform {
        identity: ctx.identity.clone(),
        input_root: ctx.staging_dir.clone(),
        staging_root: ctx.staging_dir.clone(),
    };

    let mut outcome = BatchOutcome::default();
    for uid in &batch.uids {
        let eml_path = mailbox_dir.join(format!("{uid}.eml"));
        let transformed = transformer.transform(eml_path.clone())?;
        match transformed {
            Some(transformed) if transform::verify_transformed(&transformed) => {
                manifest::append_checkpoint(
                    &ctx.staging_dir,
                    &CheckpointEntry {
                        mailbox: batch.mailbox.clone(),
                        uid: *uid,
                        message_hash: transformed.message_hash,
                        md_staged_relpath: transformed.md_staged_relpath,
                        desired_md_name: transformed.desired_md_name,
                        mailbox_tag: transformed.mailbox_tag,
                        attachments: transformed
                            .attachments
                            .into_iter()
                            .map(|attachment| (attachment.hash, attachment.staged_relpath))
                            .collect(),
                    },
                )?;
                let _ = fs::remove_file(&eml_path);
                outcome.synced += 1;
            }
            Some(_) => {
                let _ = multi_progress.println(format!(
                    "Warning: verification failed for UID {uid} in '{}', keeping {}",
                    batch.mailbox,
                    eml_path.display()
                ));
                outcome.failed += 1;
            }
            None => {
                outcome.failed += 1;
            }
        }
    }

    Ok(outcome)
}

/// One file queued for upload, carrying everything the concurrent upload
/// phase needs without re-deriving it: which identity's `.uploaded` index
/// to commit into, the absolute path to read, and its already-computed S3
/// key.
struct UploadTask {
    staging_dir: PathBuf,
    path: PathBuf,
    key: String,
}

const UPLOAD_RETRIES: usize = 3;
const UPLOAD_RETRY_BACKOFF: Duration = Duration::from_secs(2);

const UPLOADED_FILE_NAME: &str = ".uploaded";

/// Tracks which output files (by their S3 key, per `upload_key`) have
/// already been confirmed uploaded, so a resumed/re-run upload phase can
/// skip them without a redundant network round-trip. Purely a
/// resumability-speed optimization, not a correctness requirement --
/// `client::upload_if_changed`'s ETag comparison is already idempotent on
/// its own.
struct UploadedIndex {
    uploaded: HashSet<String>,
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

    fn contains(&self, key: &str) -> bool {
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

/// The S3 key for `path` (an absolute path rooted at `output_dir`):
/// `output_dir`'s relative tree mirrored directly at the bucket root, per
/// ADR-0011. Joined component-wise rather than via `to_string_lossy()` on
/// the whole relative path so the key always uses `/`, regardless of the
/// host platform's path separator. When `encrypt` is true, appends `.enc`
/// so the final key is what actually gets uploaded (ciphertext) and is what
/// `UploadedIndex`/`client::upload_if_changed` key off of -- computed once,
/// here, rather than branched again at upload time (ADR-0025).
fn upload_key(output_dir: &Path, path: &Path, encrypt: bool) -> Result<String, String> {
    let relative = path
        .strip_prefix(output_dir)
        .map_err(|_| format!("{} is not under {}", path.display(), output_dir.display()))?;
    let key = relative
        .components()
        .map(|component| component.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/");
    Ok(if encrypt { format!("{key}.enc") } else { key })
}

/// Outcome of one identity's upload phase.
#[derive(Debug, Default)]
struct UploadSummary {
    uploaded: usize,
    unchanged: usize,
    upload_failed: usize,
}

/// Builds this identity's not-yet-uploaded file list (per ADR-0019's
/// `.uploaded` tracking) and loads its index, without uploading anything --
/// kept separate from the upload itself (ADR-0024 §1) so it's unit-testable
/// without any network call, and so every identity's tasks can be gathered
/// into one shared queue before the concurrent upload phase runs.
fn pending_upload_tasks(
    ctx: &IdentityContext,
    identity_dir: &Path,
    encrypt: bool,
) -> Result<(Vec<UploadTask>, UploadedIndex), String> {
    let uploaded_index = UploadedIndex::load(&ctx.staging_dir)?;
    let mut tasks = Vec::new();
    for path in collect_files(identity_dir)? {
        let key = upload_key(&ctx.output_dir, &path, encrypt)?;
        if !uploaded_index.contains(&key) {
            tasks.push(UploadTask {
                staging_dir: ctx.staging_dir.clone(),
                path,
                key,
            });
        }
    }
    Ok((tasks, uploaded_index))
}

/// Commits a successful upload into the uploading identity's index, locked
/// only for the duration of this call -- identities never contend on each
/// other's lock, only concurrent uploads for the *same* identity do
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

/// Reads and uploads one file, retrying transient failures with backoff the
/// same way `connect_with_retry` does for IMAP (ADR-0024 §6), then commits
/// success into its identity's index and advances the shared progress bar.
/// A file that still fails after exhausting retries is warned about via
/// `multi_progress.println` (load-bearing now that upload bars are live,
/// ADR-0024 §4/ADR-0015) and counted as failed -- never committed, so it's
/// retried again on the job's next invocation.
async fn upload_one(
    task: UploadTask,
    uploaded_indexes: &HashMap<PathBuf, Arc<Mutex<UploadedIndex>>>,
    bucket_config: &BucketConfig,
    secret: &str,
    encryptor: Option<&Aes256GcmSivEncryptor>,
    bar: &ProgressBar,
    multi_progress: &MultiProgress,
) -> UploadOutcomeKind {
    let outcome = async {
        let data = fs::read(&task.path)
            .map_err(|err| format!("failed to read {}: {err}", task.path.display()))?;
        // Deterministic encryption (ADR-0025): identical plaintext always
        // yields identical ciphertext under the same key, so
        // `upload_if_changed`'s MD5-vs-ETag dedup below needs no changes.
        let data = match encryptor {
            Some(encryptor) => encryptor.encrypt(&data)?,
            None => data,
        };
        retry_with_backoff(UPLOAD_RETRIES, UPLOAD_RETRY_BACKOFF, || {
            client::upload_if_changed(bucket_config, secret, &task.key, data.clone())
        })
        .await
    }
    .await;

    bar.inc(1);
    match outcome {
        Ok(client::UploadOutcome::Uploaded) => {
            let _ = commit_uploaded(uploaded_indexes, &task);
            UploadOutcomeKind::Uploaded
        }
        Ok(client::UploadOutcome::Unchanged) => {
            let _ = commit_uploaded(uploaded_indexes, &task);
            UploadOutcomeKind::Unchanged
        }
        Err(err) => {
            let _ = multi_progress.println(format!(
                "Warning: upload failed for {}: {err}",
                task.path.display()
            ));
            UploadOutcomeKind::Failed
        }
    }
}

/// Uploads every task in `tasks` -- spanning every selected identity's
/// not-yet-uploaded files, gathered once every identity's dedup pass has
/// completed (ADR-0024 §1) -- concurrently at `concurrency`, via
/// `stream::buffer_unordered` rather than a manual worker pool: uploads
/// have no per-worker session to reuse (`client::upload_if_changed` already
/// builds a fresh S3 client per call), so there's no connection-affinity
/// reason to prefer the fetch/transform worker-pool shape here. Each task
/// is yielded by `stream::iter` exactly once, so no two concurrently
/// in-flight uploads can ever be for the same file (ADR-0024 §2).
async fn run_upload_phase(
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

/// Outcome of a full `job run email-sync` execution, across every selected
/// identity.
#[derive(Debug, Default)]
pub(crate) struct JobSummary {
    pub synced: usize,
    pub failed: usize,
    pub merged_messages: usize,
    pub deduped_attachments: usize,
    pub uploaded: usize,
    pub unchanged: usize,
    pub upload_failed: usize,
}

/// Runs the full four-phase pipeline (ADR-0021 §7) for every identity in
/// `identities`. `pending_by_identity` is each identity's already-gathered
/// `gather_pending` result, same order/index as `identities` -- the wizard
/// calls `gather_pending` itself (via `Job::gather`) to build the pre-run
/// summary, so this reuses that result instead of re-connecting to IMAP.
/// Splits every mailbox's pending UIDs into batches at `concurrency`
/// (ADR-0021 §6), then fetches+transforms them all concurrently through one
/// worker pool spanning every identity's every mailbox (this is what fully
/// fixes Context problem 1, the concurrency-capped-by-mailbox-count bug),
/// then runs each identity's dedup pass and (if `remote` is given) upload
/// phase sequentially, one identity after another.
pub(crate) async fn run_email_sync_job(
    identities: Vec<IdentityContext>,
    pending_by_identity: Vec<Vec<PendingMailbox>>,
    concurrency: usize,
    remote: Option<(&BucketConfig, &str)>,
    encryptor: Option<&Aes256GcmSivEncryptor>,
) -> Result<JobSummary, String> {
    let encrypt = encryptor.is_some();
    let mut all_batches: VecDeque<(usize, Batch)> = VecDeque::new();
    for (index, pending) in pending_by_identity.iter().enumerate() {
        let batches = super::batches_from_pending(pending, concurrency);
        all_batches.extend(batches.into_iter().map(|batch| (index, batch)));
    }

    // A fixed-size pool of `concurrency` persistent workers pulling from one
    // shared queue (ADR-0021 §6), never more than there are batches to hand
    // out.
    let worker_count = concurrency.max(1).min(all_batches.len().max(1));
    let queue = Arc::new(Mutex::new(all_batches));
    let identities = Arc::new(identities);
    let multi_progress = MultiProgress::new();

    let mut handles = Vec::with_capacity(worker_count);
    for _ in 0..worker_count {
        let queue = Arc::clone(&queue);
        let identities = Arc::clone(&identities);
        let multi_progress = multi_progress.clone();
        handles.push(tokio::spawn(run_worker(queue, identities, multi_progress)));
    }

    let mut summary = JobSummary::default();
    let mut first_error = None;
    for handle in handles {
        match handle.await {
            Ok(outcome) => {
                summary.synced += outcome.synced;
                summary.failed += outcome.failed;
            }
            Err(err) => {
                let message = format!("worker task panicked: {err}");
                eprintln!("Error: {message}");
                if first_error.is_none() {
                    first_error = Some(message);
                }
            }
        }
    }
    if let Some(err) = first_error {
        return Err(err);
    }

    let mut all_upload_tasks = Vec::new();
    let mut uploaded_indexes: HashMap<PathBuf, Arc<Mutex<UploadedIndex>>> = HashMap::new();

    for ctx in identities.iter() {
        let identity_dir = ctx
            .output_dir
            .join(identity::sanitize_segment(&ctx.identity.email));
        let mut entries = manifest::load_checkpoint(&ctx.staging_dir)?;
        let mut message_index = EmailDedup(ContentIndex::load(
            &ctx.staging_dir,
            transform::MESSAGE_HASHES_FILE,
        )?);
        let mut attachment_index = EmailDedup(ContentIndex::load(
            &ctx.staging_dir,
            transform::ATTACHMENT_HASHES_FILE,
        )?);
        let dedup_summary = dedup::run_dedup_pass(
            &identity_dir,
            &ctx.staging_dir,
            &mut entries,
            &mut message_index,
            &mut attachment_index,
        )?;
        summary.merged_messages += dedup_summary.merged_messages;
        summary.deduped_attachments += dedup_summary.deduped_attachments;

        if remote.is_some() {
            let (tasks, uploaded_index) = pending_upload_tasks(ctx, &identity_dir, encrypt)?;
            all_upload_tasks.extend(tasks);
            uploaded_indexes.insert(
                ctx.staging_dir.clone(),
                Arc::new(Mutex::new(uploaded_index)),
            );
        }
    }

    if let Some((bucket_config, secret)) = remote {
        let upload_summary = run_upload_phase(
            all_upload_tasks,
            &uploaded_indexes,
            bucket_config,
            secret,
            encryptor,
            concurrency,
            &multi_progress,
        )
        .await;
        summary.uploaded += upload_summary.uploaded;
        summary.unchanged += upload_summary.unchanged;
        summary.upload_failed += upload_summary.upload_failed;
    }

    Ok(summary)
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    #[tokio::test]
    async fn retry_with_backoff_succeeds_after_transient_failures() {
        let attempts = AtomicUsize::new(0);
        let result: Result<&str, String> =
            retry_with_backoff(CONNECT_RETRIES, Duration::from_millis(1), || {
                let count = attempts.fetch_add(1, Ordering::SeqCst) + 1;
                async move {
                    if count < 3 {
                        Err(format!("attempt {count} failed"))
                    } else {
                        Ok("connected")
                    }
                }
            })
            .await;

        assert_eq!(result, Ok("connected"));
        assert_eq!(attempts.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn retry_with_backoff_returns_the_last_error_after_exhausting_attempts() {
        let attempts = AtomicUsize::new(0);
        let result: Result<(), String> =
            retry_with_backoff(CONNECT_RETRIES, Duration::from_millis(1), || {
                let count = attempts.fetch_add(1, Ordering::SeqCst) + 1;
                async move { Err(format!("attempt {count} failed")) }
            })
            .await;

        assert_eq!(result, Err("attempt 3 failed".to_string()));
        assert_eq!(attempts.load(Ordering::SeqCst), CONNECT_RETRIES);
    }

    #[tokio::test]
    async fn retry_with_backoff_does_not_retry_a_first_success() {
        let attempts = AtomicUsize::new(0);
        let result: Result<&str, String> =
            retry_with_backoff(CONNECT_RETRIES, Duration::from_millis(1), || {
                attempts.fetch_add(1, Ordering::SeqCst);
                async move { Ok("connected") }
            })
            .await;

        assert_eq!(result, Ok("connected"));
        assert_eq!(attempts.load(Ordering::SeqCst), 1);
    }

    fn test_ctx(staging_dir: &Path, output_dir: &Path) -> IdentityContext {
        IdentityContext {
            identity: crate::commands::keyring::email::identity::Identity {
                alias: "alias".to_string(),
                email: "person@example.com".to_string(),
                provider: crate::commands::keyring::email::provider::Provider::Gmail,
                host: "imap.gmail.com".to_string(),
                port: 993,
            },
            secret: "secret".to_string(),
            staging_dir: staging_dir.to_path_buf(),
            output_dir: output_dir.to_path_buf(),
        }
    }

    #[test]
    fn pending_upload_tasks_includes_every_file_when_index_is_empty() {
        let staging = tempfile::tempdir().unwrap();
        let output = tempfile::tempdir().unwrap();
        let identity_dir = output.path().join("alias-out");
        fs::create_dir_all(&identity_dir).unwrap();
        fs::write(identity_dir.join("a.md"), b"a").unwrap();
        fs::write(identity_dir.join("b.md"), b"b").unwrap();

        let ctx = test_ctx(staging.path(), output.path());
        let (tasks, index) = pending_upload_tasks(&ctx, &identity_dir, false).unwrap();

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

        let ctx = test_ctx(staging.path(), output.path());
        let key_a = upload_key(&ctx.output_dir, &identity_dir.join("a.md"), false).unwrap();
        fs::write(
            staging.path().join(UPLOADED_FILE_NAME),
            format!("{key_a}\n"),
        )
        .unwrap();

        let (tasks, index) = pending_upload_tasks(&ctx, &identity_dir, false).unwrap();

        assert_eq!(tasks.len(), 1);
        assert_eq!(
            tasks[0].key,
            upload_key(&ctx.output_dir, &identity_dir.join("b.md"), false).unwrap()
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

        let ctx = test_ctx(staging.path(), output.path());
        let (tasks, _index) = pending_upload_tasks(&ctx, &identity_dir, true).unwrap();

        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].key, "alias-out/a.md.enc");
    }
}
