use std::collections::{HashSet, VecDeque};
use std::fs;
use std::future::Future;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use indicatif::MultiProgress;

use crate::commands::keyring::bucket::client;
use crate::commands::keyring::bucket::store::BucketConfig;
use crate::commands::keyring::email::identity;
use crate::commands::keyring::email::imap_client::{self, ImapSession};
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
/// host platform's path separator.
fn upload_key(output_dir: &Path, path: &Path) -> Result<String, String> {
    let relative = path
        .strip_prefix(output_dir)
        .map_err(|_| format!("{} is not under {}", path.display(), output_dir.display()))?;
    Ok(relative
        .components()
        .map(|component| component.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/"))
}

/// Outcome of one identity's upload phase.
#[derive(Debug, Default)]
struct UploadSummary {
    uploaded: usize,
    unchanged: usize,
    upload_failed: usize,
}

/// Uploads every file under `identity_dir` that isn't already recorded in
/// `.uploaded`, per ADR-0019 -- runs only after this identity's dedup pass
/// has fully completed, so every file it sees is already in its final,
/// deduped state.
async fn run_upload_phase(
    identity_dir: &Path,
    output_dir: &Path,
    staging_dir: &Path,
    bucket_config: &BucketConfig,
    secret: &str,
) -> Result<UploadSummary, String> {
    let mut uploaded_index = UploadedIndex::load(staging_dir)?;
    let files = collect_files(identity_dir)?;

    let mut summary = UploadSummary::default();
    for path in files {
        let key = upload_key(output_dir, &path)?;
        if uploaded_index.contains(&key) {
            continue;
        }

        let data =
            fs::read(&path).map_err(|err| format!("failed to read {}: {err}", path.display()))?;
        match client::upload_if_changed(bucket_config, secret, &key, data).await {
            Ok(client::UploadOutcome::Uploaded) => summary.uploaded += 1,
            Ok(client::UploadOutcome::Unchanged) => summary.unchanged += 1,
            Err(err) => {
                eprintln!("Warning: upload failed for {}: {err}", path.display());
                summary.upload_failed += 1;
                continue;
            }
        }
        uploaded_index.commit(staging_dir, &key)?;
    }
    Ok(summary)
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
) -> Result<JobSummary, String> {
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

        if let Some((bucket_config, secret)) = remote {
            let upload_summary = run_upload_phase(
                &identity_dir,
                &ctx.output_dir,
                &ctx.staging_dir,
                bucket_config,
                secret,
            )
            .await?;
            summary.uploaded += upload_summary.uploaded;
            summary.unchanged += upload_summary.unchanged;
            summary.upload_failed += upload_summary.upload_failed;
        }
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
}
