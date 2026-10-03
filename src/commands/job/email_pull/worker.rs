use std::collections::{HashMap, VecDeque};
use std::fs;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use indicatif::MultiProgress;
use mail_parser::{MessageParser, MimeHeaders};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use crate::commands::job::email_sync::sink;
use crate::commands::job::email_sync::{IdentityContext, manifest::Batch};
use crate::commands::job::upload::{self, UploadedIndex};
use crate::commands::keyring::bucket::store::BucketConfig;
use crate::commands::keyring::email::identity;
use crate::commands::keyring::email::imap_client::{self, ImapSession};
use crate::core::data::{ContentIndex, sanitize_filename};
use crate::core::retry::retry_with_backoff;

use super::dedup::{self, ATTACHMENT_HASHES_FILE, EmailPullDedup};
use super::manifest::{self, PullCheckpointEntry};

const CONNECT_RETRIES: usize = 3;
const RETRY_BACKOFF: Duration = Duration::from_secs(5);

/// How many extra times a batch that failed to connect/`EXAMINE`/fetch is
/// requeued before its UIDs are counted as failed for this run -- same
/// shape as `email_sync::worker`'s (ADR-0071), duplicated here since
/// `email_sync::worker` is a private module and unreachable from this
/// sibling job (ADR-0081's own "second consumer still duplicates" call).
const BATCH_RETRIES: u8 = 2;
const BATCH_RETRY_BACKOFF: Duration = Duration::from_secs(2);

/// Connects and logs in to `ctx`'s identity, retrying on failure. A small,
/// deliberate duplicate of `email_sync::worker::connect_with_retry` --
/// that function is `pub(crate)` but `email_sync::worker` itself is a
/// private module (`mod worker;`), so it isn't actually reachable from
/// here.
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

/// A `failed` count broken down by cause. Unlike
/// `email_sync::worker::FailureBreakdown`, there's no `verification`/
/// `parse_skipped` category -- a message parse failure never drops the
/// already-fetched `.eml` or its checkpoint entry here (see
/// `process_batch_on_session`'s doc comment), so it's tracked separately,
/// as an informational `attachment_extraction_failed` count on
/// `BatchOutcome`/`JobSummary`, not as part of `failed` at all.
#[derive(Debug, Default)]
pub(crate) struct FailureBreakdown {
    pub connect: usize,
    pub examine: usize,
    pub batch_error: usize,
    pub missing_file: usize,
}

impl FailureBreakdown {
    fn merge(&mut self, other: &FailureBreakdown) {
        self.connect += other.connect;
        self.examine += other.examine;
        self.batch_error += other.batch_error;
        self.missing_file += other.missing_file;
    }
}

/// Outcome of one worker's processing, for the job-level summary.
#[derive(Default)]
struct BatchOutcome {
    synced: usize,
    failed: usize,
    attachments_staged: usize,
    attachment_extraction_failed: usize,
    failure_breakdown: FailureBreakdown,
}

/// A worker's currently-open IMAP session, if any -- same role as
/// `email_sync::worker::WorkerConnection`.
struct WorkerConnection {
    identity_index: usize,
    mailbox: String,
    session: ImapSession,
    _permit: OwnedSemaphorePermit,
}

/// After a batch fails, either sleeps briefly and returns a requeue-able
/// item with one fewer attempt remaining, or `None` once retries are
/// exhausted. Identical shape to `email_sync::worker::requeue_or_none`.
async fn requeue_or_none(
    identity_index: usize,
    batch: Batch,
    attempts_remaining: u8,
    backoff: Duration,
) -> Option<(usize, Batch, u8)> {
    if attempts_remaining == 0 {
        return None;
    }
    tokio::time::sleep(backoff).await;
    Some((identity_index, batch, attempts_remaining - 1))
}

/// Pulls `(identity_index, Batch, attempts_remaining)` items from `queue`
/// until it's drained, holding one IMAP session per identity for as long as
/// consecutive batches it pulls belong to that identity -- same connection-
/// reuse/per-identity-cap shape as `email_sync::worker::run_worker`
/// (ADR-0021 §6 addendum, ADR-0071).
async fn run_worker(
    queue: Arc<Mutex<VecDeque<(usize, Batch, u8)>>>,
    identities: Arc<Vec<IdentityContext>>,
    identity_semaphores: Arc<Vec<Arc<Semaphore>>>,
    multi_progress: MultiProgress,
) -> BatchOutcome {
    let mut connection: Option<WorkerConnection> = None;
    let mut outcome = BatchOutcome::default();

    loop {
        let next = { queue.lock().unwrap().pop_front() };
        let Some((identity_index, batch, attempts_remaining)) = next else {
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
            let permit = identity_semaphores[identity_index]
                .clone()
                .acquire_owned()
                .await
                .expect("identity semaphores are never closed");
            match connect_with_retry(ctx).await {
                Ok(session) => {
                    crate::observability::metrics::record_phase("email-pull", "connect", "ok");
                    connection = Some(WorkerConnection {
                        identity_index,
                        mailbox: String::new(),
                        session,
                        _permit: permit,
                    });
                }
                Err(err) => {
                    let _ = multi_progress.println(format!("Error: {err}"));
                    let uid_count = batch.uids.len();
                    let mailbox = batch.mailbox.clone();
                    match requeue_or_none(
                        identity_index,
                        batch,
                        attempts_remaining,
                        BATCH_RETRY_BACKOFF,
                    )
                    .await
                    {
                        Some(item) => {
                            tracing::warn!(
                                identity = %ctx.identity.alias,
                                mailbox,
                                step = "connect",
                                attempts_remaining = item.2,
                                "requeueing batch after connect failure"
                            );
                            queue.lock().unwrap().push_back(item)
                        }
                        None => {
                            tracing::error!(
                                identity = %ctx.identity.alias,
                                mailbox,
                                step = "connect",
                                uid_count,
                                "batch retries exhausted, dropping"
                            );
                            outcome.failed += uid_count;
                            outcome.failure_breakdown.connect += uid_count;
                            crate::observability::metrics::record_phase_count(
                                "email-pull",
                                "connect",
                                "failed",
                                uid_count as u64,
                            );
                        }
                    }
                    continue;
                }
            }
        }

        let conn = connection.as_mut().unwrap();
        if conn.mailbox != batch.mailbox {
            match conn.session.examine(&batch.mailbox).await {
                Ok(_) => {
                    conn.mailbox = batch.mailbox.clone();
                    crate::observability::metrics::record_phase("email-pull", "examine", "ok");
                }
                Err(err) => {
                    let _ = multi_progress.println(format!(
                        "Error: failed to open '{}' read-only: {err}",
                        batch.mailbox
                    ));
                    connection = None;
                    let uid_count = batch.uids.len();
                    let mailbox = batch.mailbox.clone();
                    match requeue_or_none(
                        identity_index,
                        batch,
                        attempts_remaining,
                        BATCH_RETRY_BACKOFF,
                    )
                    .await
                    {
                        Some(item) => {
                            tracing::warn!(
                                identity = %ctx.identity.alias,
                                mailbox,
                                step = "examine",
                                attempts_remaining = item.2,
                                "requeueing batch after EXAMINE failure"
                            );
                            queue.lock().unwrap().push_back(item)
                        }
                        None => {
                            tracing::error!(
                                identity = %ctx.identity.alias,
                                mailbox,
                                step = "examine",
                                uid_count,
                                "batch retries exhausted, dropping"
                            );
                            outcome.failed += uid_count;
                            outcome.failure_breakdown.examine += uid_count;
                            crate::observability::metrics::record_phase_count(
                                "email-pull",
                                "examine",
                                "failed",
                                uid_count as u64,
                            );
                        }
                    }
                    continue;
                }
            }
        }

        let conn = connection.as_mut().unwrap();
        match process_batch_on_session(ctx, &batch, &mut conn.session, &multi_progress).await {
            Ok(batch_outcome) => {
                outcome.synced += batch_outcome.synced;
                outcome.failed += batch_outcome.failed;
                outcome.attachments_staged += batch_outcome.attachments_staged;
                outcome.attachment_extraction_failed += batch_outcome.attachment_extraction_failed;
                outcome
                    .failure_breakdown
                    .merge(&batch_outcome.failure_breakdown);
            }
            Err(err) => {
                let _ = multi_progress.println(format!("Error: {err}"));
                connection = None;
                let uid_count = batch.uids.len();
                let mailbox = batch.mailbox.clone();
                match requeue_or_none(
                    identity_index,
                    batch,
                    attempts_remaining,
                    BATCH_RETRY_BACKOFF,
                )
                .await
                {
                    Some(item) => {
                        tracing::warn!(
                            identity = %ctx.identity.alias,
                            mailbox,
                            step = "batch",
                            attempts_remaining = item.2,
                            "requeueing batch after processing failure"
                        );
                        queue.lock().unwrap().push_back(item)
                    }
                    None => {
                        tracing::error!(
                            identity = %ctx.identity.alias,
                            mailbox,
                            step = "batch",
                            uid_count,
                            "batch retries exhausted, dropping"
                        );
                        outcome.failed += uid_count;
                        outcome.failure_breakdown.batch_error += uid_count;
                        crate::observability::metrics::record_phase_count(
                            "email-pull",
                            "batch_error",
                            "failed",
                            uid_count as u64,
                        );
                    }
                }
            }
        }
    }

    if let Some(mut conn) = connection {
        let _ = conn.session.logout().await;
    }

    outcome
}

/// Fetches every UID in `batch` straight to its final location (no
/// staging/rename step -- `(mailbox, uid)` is already unique, per
/// ADR-0081 §3) and, best-effort, extracts each message's attachments into
/// a UID-keyed scratch directory awaiting the single-threaded dedup/
/// placement pass.
///
/// A message that fails to parse (`mail_parser::MessageParser::parse`
/// returning `None`) does **not** drop the `.eml` or skip its checkpoint
/// entry -- unlike `email_sync::transform::EmailTransform`, this job's
/// primary deliverable is the raw `.eml` itself, already safely on disk by
/// the time parsing is even attempted, and filenames here are UID-based,
/// not date/subject-derived, so nothing about the fetch depends on a
/// successful parse. A parse failure is logged and counted in
/// `attachment_extraction_failed` (informational, not part of `failed`),
/// and the UID is still checkpointed with zero attachments.
async fn process_batch_on_session(
    ctx: &IdentityContext,
    batch: &Batch,
    session: &mut ImapSession,
    multi_progress: &MultiProgress,
) -> Result<BatchOutcome, String> {
    let identity_dir = ctx
        .output_dir
        .join(identity::sanitize_segment(&ctx.identity.email));
    let final_mailbox_dir = identity_dir.join(&batch.mailbox_relpath);
    fs::create_dir_all(&final_mailbox_dir)
        .map_err(|err| format!("failed to create {}: {err}", final_mailbox_dir.display()))?;

    sink::fetch_uids(
        session,
        &batch.mailbox,
        &final_mailbox_dir,
        &batch.uids,
        multi_progress,
    )
    .await?;

    let mut outcome = BatchOutcome::default();
    for uid in &batch.uids {
        let eml_path = final_mailbox_dir.join(format!("{uid}.eml"));
        if !eml_path.exists() {
            tracing::warn!(uid, mailbox = %batch.mailbox, step = "fetch", "eml missing after fetch, counted as failed");
            outcome.failed += 1;
            outcome.failure_breakdown.missing_file += 1;
            crate::observability::metrics::record_phase("email-pull", "missing_file", "failed");
            continue;
        }

        let attachments = multi_progress.suspend(|| {
            extract_attachments(
                &eml_path,
                &ctx.staging_dir,
                &batch.mailbox_relpath,
                *uid,
                &batch.mailbox,
            )
        });
        match attachments {
            Ok(attachments) => {
                outcome.attachments_staged += attachments.len();
                manifest::append_checkpoint(
                    &ctx.staging_dir,
                    &PullCheckpointEntry {
                        mailbox: batch.mailbox.clone(),
                        uid: *uid,
                        attachments,
                    },
                )?;
                outcome.synced += 1;
                crate::observability::metrics::record_phase(
                    "email-pull",
                    "attachment_extraction",
                    "extracted",
                );
            }
            Err(()) => {
                outcome.attachment_extraction_failed += 1;
                manifest::append_checkpoint(
                    &ctx.staging_dir,
                    &PullCheckpointEntry {
                        mailbox: batch.mailbox.clone(),
                        uid: *uid,
                        attachments: Vec::new(),
                    },
                )?;
                outcome.synced += 1;
                // Still counted as "synced" (ADR-0093) -- a parse failure
                // here doesn't drop the raw .eml or skip its checkpoint,
                // unlike email_sync's equivalent, per this job's own doc
                // comment above.
                crate::observability::metrics::record_phase(
                    "email-pull",
                    "attachment_extraction",
                    "extraction_failed",
                );
            }
        }
    }

    Ok(outcome)
}

/// Parses `eml_path` and stages each attachment part's bytes under
/// `staging_dir/<mailbox_relpath>/<uid>/attachments/`, returning `(hash,
/// staging-root-relative-path)` pairs. `Err(())` means the message
/// couldn't be parsed at all -- a lenient, logged condition, not a hard
/// error (see `process_batch_on_session`'s doc comment).
fn extract_attachments(
    eml_path: &std::path::Path,
    staging_dir: &std::path::Path,
    mailbox_relpath: &std::path::Path,
    uid: u32,
    mailbox: &str,
) -> Result<Vec<(String, String)>, ()> {
    let bytes = match fs::read(eml_path) {
        Ok(bytes) => bytes,
        Err(err) => {
            tracing::warn!(uid, mailbox, step = "extract", error = %err, "failed to read eml file");
            return Err(());
        }
    };

    let Some(message) = MessageParser::default().parse(&bytes) else {
        tracing::warn!(
            uid,
            mailbox,
            step = "extract",
            "failed to parse eml file, keeping raw file with no attachments extracted"
        );
        return Err(());
    };

    let attachments_dir = staging_dir
        .join(mailbox_relpath)
        .join(uid.to_string())
        .join("attachments");
    let mut staged = Vec::new();
    for part in message.attachments() {
        let contents = part.contents();
        let name = part.attachment_name();
        if contents.is_empty() && name.is_none() {
            // A truncated/malformed trailing MIME part with no headers and
            // no content -- not a real attachment, same guard
            // `EmailTransform::transform` applies (ADR-0030 amendment).
            continue;
        }
        if let Err(err) = fs::create_dir_all(&attachments_dir) {
            tracing::warn!(uid, mailbox, step = "extract", error = %err, "failed to create attachment scratch dir");
            continue;
        }
        let hash = format!("{:x}", md5::compute(contents));
        let sanitized_name = sanitize_filename(name.unwrap_or("attachment"));
        let staged_path = crate::core::data::unique_path(&attachments_dir.join(&sanitized_name));
        if let Err(err) = fs::write(&staged_path, contents) {
            tracing::warn!(uid, mailbox, step = "extract", error = %err, "failed to write staged attachment");
            continue;
        }
        let relpath = relpath_string(staging_dir, &staged_path);
        staged.push((hash, relpath));
    }

    Ok(staged)
}

/// `path`'s location relative to `base`, joined with `/` regardless of the
/// host platform's path separator.
fn relpath_string(base: &std::path::Path, path: &std::path::Path) -> String {
    path.strip_prefix(base)
        .unwrap_or(path)
        .components()
        .map(|component| component.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}

/// Outcome of a full `job run email-pull` execution, across every selected
/// identity.
#[derive(Debug, Default)]
pub(crate) struct JobSummary {
    pub synced: usize,
    pub failed: usize,
    pub failure_breakdown: FailureBreakdown,
    pub attachments_staged: usize,
    pub attachment_extraction_failed: usize,
    pub deduped_attachments: usize,
    pub uploaded: usize,
    pub unchanged: usize,
    pub upload_failed: usize,
}

/// Runs the full pipeline for every identity in `identities`: concurrent
/// fetch+extract, then each identity's dedup/placement pass, then (if
/// `remote` is given) a shared concurrent upload phase -- always
/// unencrypted (ADR-0081 §4; no `Encryptor` is ever constructed for this
/// job). Mirrors `email_sync::worker::run_email_sync_job`'s structure.
pub(crate) async fn run_email_pull_job(
    identities: Vec<IdentityContext>,
    pending_by_identity: Vec<Vec<crate::commands::job::email_sync::PendingMailbox>>,
    concurrency: usize,
    upload_concurrency: usize,
    max_connections_per_identity: usize,
    remote: Option<(&BucketConfig, &str)>,
) -> Result<JobSummary, String> {
    crate::observability::metrics::set_macro_phase("email-pull", false);
    let mut per_identity_batches: Vec<VecDeque<Batch>> = pending_by_identity
        .iter()
        .map(|pending| {
            crate::commands::job::email_sync::batches_from_pending(pending, concurrency).into()
        })
        .collect();
    let mut all_batches: VecDeque<(usize, Batch, u8)> = VecDeque::new();
    loop {
        let mut any_left = false;
        for (index, batches) in per_identity_batches.iter_mut().enumerate() {
            if let Some(batch) = batches.pop_front() {
                all_batches.push_back((index, batch, BATCH_RETRIES));
                any_left = true;
            }
        }
        if !any_left {
            break;
        }
    }

    let worker_count = concurrency.max(1).min(all_batches.len().max(1));
    let queue = Arc::new(Mutex::new(all_batches));
    let identities = Arc::new(identities);
    let identity_semaphores: Arc<Vec<Arc<Semaphore>>> = Arc::new(
        identities
            .iter()
            .map(|ctx| {
                let cap = ctx
                    .identity
                    .max_imap_connections
                    .map(|value| value as usize)
                    .unwrap_or(max_connections_per_identity);
                Arc::new(Semaphore::new(concurrency.min(cap).max(1)))
            })
            .collect(),
    );
    let multi_progress = MultiProgress::new();

    let mut handles = Vec::with_capacity(worker_count);
    for _ in 0..worker_count {
        let queue = Arc::clone(&queue);
        let identities = Arc::clone(&identities);
        let identity_semaphores = Arc::clone(&identity_semaphores);
        let multi_progress = multi_progress.clone();
        handles.push(tokio::spawn(run_worker(
            queue,
            identities,
            identity_semaphores,
            multi_progress,
        )));
    }

    let mut summary = JobSummary::default();
    let mut first_error = None;
    for handle in handles {
        match handle.await {
            Ok(outcome) => {
                summary.synced += outcome.synced;
                summary.failed += outcome.failed;
                summary.attachments_staged += outcome.attachments_staged;
                summary.attachment_extraction_failed += outcome.attachment_extraction_failed;
                summary.failure_breakdown.merge(&outcome.failure_breakdown);
            }
            Err(err) => {
                let message = format!("worker task panicked: {err}");
                let _ = multi_progress.println(format!("Error: {message}"));
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
        let mut attachment_index = EmailPullDedup(ContentIndex::load(
            &ctx.staging_dir,
            ATTACHMENT_HASHES_FILE,
        )?);
        let dedup_summary = dedup::place_attachments(
            &identity_dir,
            &ctx.staging_dir,
            &mut entries,
            &mut attachment_index,
            &multi_progress,
        )?;
        summary.deduped_attachments += dedup_summary.duplicates_skipped;

        if remote.is_some() {
            let (tasks, uploaded_index) = identity_upload_tasks(ctx)?;
            all_upload_tasks.extend(tasks);
            uploaded_indexes.insert(
                ctx.staging_dir.clone(),
                Arc::new(Mutex::new(uploaded_index)),
            );
        }
    }

    if let Some((bucket_config, secret)) = remote {
        let upload_summary = upload::run_upload_phase(
            all_upload_tasks,
            &uploaded_indexes,
            bucket_config,
            secret,
            None,
            upload_concurrency,
            &multi_progress,
        )
        .await;
        summary.uploaded += upload_summary.uploaded;
        summary.unchanged += upload_summary.unchanged;
        summary.upload_failed += upload_summary.upload_failed;
    }

    Ok(summary)
}

/// Builds `ctx`'s pending upload tasks against its already-placed
/// `identity_dir` -- the exact per-identity upload-task-building step
/// `run_email_pull_job`'s own loop already does (after its dedup pass);
/// `run_upload_only` calls this directly for every selected identity,
/// skipping the dedup pass entirely since a prior run already completed it
/// (ADR-0090, same ADR-0089 precedent). Always unencrypted (`false`) --
/// `email-pull` never offers encryption at all (ADR-0081 §4).
fn identity_upload_tasks(
    ctx: &IdentityContext,
) -> Result<(Vec<upload::UploadTask>, UploadedIndex), String> {
    let identity_dir = ctx
        .output_dir
        .join(identity::sanitize_segment(&ctx.identity.email));
    upload::pending_upload_tasks(
        "email-pull",
        &ctx.identity.alias,
        &ctx.staging_dir,
        &identity_dir,
        &ctx.output_dir,
        false,
    )
}

/// Resumes uploading already-completed local email-pull runs, skipping the
/// IMAP connect/fetch/dedup phases entirely (ADR-0090's `--upload-only`) --
/// `identities` is expected to already be filtered down to ones with a
/// completed local run (the wizard's job, same per-identity preflight
/// `email_sync::wizard`'s version does). Accumulates every identity's
/// upload tasks into one shared `run_upload_phase` call, exactly mirroring
/// how `run_email_pull_job`'s own per-identity loop already does before its
/// own shared upload call.
pub(crate) async fn run_upload_only(
    identities: &[IdentityContext],
    remote: (&BucketConfig, &str),
    upload_concurrency: usize,
) -> Result<upload::UploadSummary, String> {
    let multi_progress = MultiProgress::new();
    let (bucket_config, secret) = remote;

    let mut all_upload_tasks = Vec::new();
    let mut uploaded_indexes: HashMap<PathBuf, Arc<Mutex<UploadedIndex>>> = HashMap::new();
    for ctx in identities {
        let (tasks, uploaded_index) = identity_upload_tasks(ctx)?;
        all_upload_tasks.extend(tasks);
        uploaded_indexes.insert(
            ctx.staging_dir.clone(),
            Arc::new(Mutex::new(uploaded_index)),
        );
    }

    Ok(upload::run_upload_phase(
        all_upload_tasks,
        &uploaded_indexes,
        bucket_config,
        secret,
        None,
        upload_concurrency,
        &multi_progress,
    )
    .await)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_batch() -> Batch {
        Batch {
            mailbox: "INBOX".to_string(),
            mailbox_relpath: PathBuf::from("inbox"),
            uids: vec![1, 2, 3],
        }
    }

    #[tokio::test]
    async fn requeue_or_none_returns_none_once_attempts_are_exhausted() {
        let result = requeue_or_none(0, test_batch(), 0, Duration::from_millis(1)).await;

        assert!(result.is_none());
    }

    #[tokio::test]
    async fn requeue_or_none_requeues_with_one_fewer_attempt_remaining() {
        let (identity_index, batch, attempts_remaining) =
            requeue_or_none(2, test_batch(), BATCH_RETRIES, Duration::from_millis(1))
                .await
                .unwrap();

        assert_eq!(identity_index, 2);
        assert_eq!(batch.uids, vec![1, 2, 3]);
        assert_eq!(attempts_remaining, BATCH_RETRIES - 1);
    }

    #[test]
    fn extract_attachments_stages_an_attachment_and_returns_its_hash() {
        let input = tempfile::tempdir().unwrap();
        let staging = tempfile::tempdir().unwrap();
        let eml_path = input.path().join("1.eml");
        std::fs::write(
            &eml_path,
            "From: a@example.com\r\n\
             To: b@example.com\r\n\
             Subject: Shipping\r\n\
             Date: Fri, 26 Jan 2024 09:15:00 +0000\r\n\
             MIME-Version: 1.0\r\n\
             Content-Type: multipart/mixed; boundary=\"B\"\r\n\
             \r\n\
             --B\r\n\
             Content-Type: text/plain\r\n\
             \r\n\
             hi\r\n\
             --B\r\n\
             Content-Type: application/pdf\r\n\
             Content-Disposition: attachment; filename=\"a.pdf\"\r\n\
             Content-Transfer-Encoding: base64\r\n\
             \r\n\
             JVBERi0xLjQK\r\n\
             --B--\r\n",
        )
        .unwrap();

        let staged = extract_attachments(
            &eml_path,
            staging.path(),
            std::path::Path::new("inbox"),
            1,
            "INBOX",
        )
        .unwrap();

        assert_eq!(staged.len(), 1);
        let (_, relpath) = &staged[0];
        assert!(staging.path().join(relpath).exists());
    }

    #[test]
    fn extract_attachments_returns_err_for_unparseable_input() {
        let input = tempfile::tempdir().unwrap();
        let staging = tempfile::tempdir().unwrap();
        let eml_path = input.path().join("missing.eml");

        let result = extract_attachments(
            &eml_path,
            staging.path(),
            std::path::Path::new("inbox"),
            5,
            "INBOX",
        );

        assert!(result.is_err());
    }
}
