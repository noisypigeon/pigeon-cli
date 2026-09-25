use std::collections::{HashMap, HashSet, VecDeque};
use std::fs;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_imap::types::NameAttribute;
use futures::TryStreamExt;
use indicatif::MultiProgress;

use crate::commands::FAILURE_EXIT_CODE;
use crate::dataops::client;
use crate::dataops::dedup::{self, ContentIndex};
use crate::dataops::store::BucketConfig;
use crate::dataops::transform::unique_path;
use crate::email::identity::{self, Identity};
use crate::email::imap_client::{self, ImapSession};
use crate::email::sink;
use crate::email::transform;
use crate::job::manifest::{self, Batch, CheckpointEntry, ManifestEntry};
use crate::keyring::credentials;
use crate::keyring::store::Store;

/// Entry point for `pigeon job run email-sync` -- the wizard flow of
/// ADR-0021 §5: resolve identities → pull/load each one's manifest → show
/// the summary and resolve concurrency (with a time estimate) → confirm →
/// run the four-phase pipeline.
pub fn dispatch(
    identities: Option<Vec<String>>,
    local_output: Option<PathBuf>,
    remote_output: Option<String>,
    concurrency: Option<usize>,
    yes: bool,
) -> i32 {
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(err) => return fail(format!("failed to start async runtime: {err}")),
    };
    runtime.block_on(dispatch_async(
        identities,
        local_output,
        remote_output,
        concurrency,
        yes,
    ))
}

async fn dispatch_async(
    identities: Option<Vec<String>>,
    local_output: Option<PathBuf>,
    remote_output: Option<String>,
    concurrency: Option<usize>,
    yes: bool,
) -> i32 {
    let keyring_store_path = match Store::default_path() {
        Ok(path) => path,
        Err(err) => return fail(err),
    };
    let keyring_store = match Store::load(&keyring_store_path) {
        Ok(store) => store,
        Err(err) => return fail(err),
    };
    let selected_identities =
        match crate::job::wizard::resolve_identities(&keyring_store, identities) {
            Ok(identities) => identities,
            Err(err) => return fail(err),
        };

    let local_output = match crate::job::wizard::resolve_local_output(local_output) {
        Ok(path) => path,
        Err(err) => return fail(err),
    };

    let mut contexts = Vec::new();
    for identity in &selected_identities {
        let secret = match credentials::get_secret(&identity.alias) {
            Ok(secret) => secret,
            Err(err) => return fail(err),
        };
        let identity_root = local_output.join(&identity.alias);
        contexts.push(IdentityContext {
            identity: identity.clone(),
            secret,
            staging_dir: identity_root.join("staging"),
            output_dir: identity_root.join("result"),
        });
    }

    let mut pending_by_identity = Vec::with_capacity(contexts.len());
    let mut manifest_summaries = Vec::with_capacity(contexts.len());
    for ctx in &contexts {
        let (pending, summary) = match gather_pending(ctx).await {
            Ok(result) => result,
            Err(err) => return fail(err),
        };
        pending_by_identity.push(pending);
        manifest_summaries.push(summary);
    }

    crate::job::wizard::print_manifest_summary(&manifest_summaries);
    let total_pending: usize = manifest_summaries
        .iter()
        .map(|summary| summary.pending_messages)
        .sum();
    if total_pending == 0 {
        println!("Everything is already up to date.");
        return 0;
    }

    let resolved_remote_alias =
        match crate::job::wizard::resolve_remote_output(remote_output, &keyring_store) {
            Ok(alias) => alias,
            Err(err) => return fail(err),
        };
    let resolved_remote: Option<(BucketConfig, String)> = match resolved_remote_alias {
        Some(alias) => {
            let bucket_config = match keyring_store.bucket_configs().find(|b| b.alias == alias) {
                Some(bucket_config) => bucket_config.clone(),
                None => return fail(format!("no bucket-config named '{alias}'")),
            };
            let secret = match credentials::get_secret(&bucket_config.alias) {
                Ok(secret) => secret,
                Err(err) => return fail(err),
            };
            Some((bucket_config, secret))
        }
        None => None,
    };

    let concurrency = match crate::job::wizard::resolve_concurrency(concurrency, total_pending) {
        Ok(value) => value,
        Err(err) => return fail(err),
    };

    match crate::job::wizard::confirm_and_proceed(yes) {
        Ok(true) => {}
        Ok(false) => {
            println!("Cancelled.");
            return 0;
        }
        Err(err) => return fail(err),
    }

    let remote_ref = resolved_remote
        .as_ref()
        .map(|(bucket_config, secret)| (bucket_config, secret.as_str()));
    match run_email_sync_job(contexts, pending_by_identity, concurrency, remote_ref).await {
        Ok(summary) => {
            println!(
                "Synced {} message(s), {} failed, {} message(s) merged, {} attachment(s) deduped, {} uploaded, {} unchanged, {} upload failed.",
                summary.synced,
                summary.failed,
                summary.merged_messages,
                summary.deduped_attachments,
                summary.uploaded,
                summary.unchanged,
                summary.upload_failed
            );
            // A worker absorbing a connect/fetch failure into `failed`
            // (ADR-0021 §6 addendum) lets the run complete and checkpoint
            // everything that succeeded, but that must still be visible to
            // a script checking the exit code -- otherwise a partially
            // failed run would silently report success.
            if summary.failed > 0 || summary.upload_failed > 0 {
                FAILURE_EXIT_CODE
            } else {
                0
            }
        }
        Err(err) => fail(err),
    }
}

fn fail(message: impl std::fmt::Display) -> i32 {
    eprintln!("Error: {message}");
    FAILURE_EXIT_CODE
}

/// Everything needed to run one identity through the job: its IMAP
/// credentials and its own isolated subtree under the job's shared
/// `--local-output` root (ADR-0021 §2 -- each identity gets
/// `<local-output>/<alias>/{staging,result}`, preserving today's
/// per-identity dedup/checkpoint scoping; nothing in ADR-0021 asks for
/// cross-identity dedup).
pub(crate) struct IdentityContext {
    pub identity: Identity,
    pub secret: String,
    pub staging_dir: PathBuf,
    pub output_dir: PathBuf,
}

/// Per-identity manifest summary, for the wizard's pre-run display
/// (ADR-0021 §5).
#[derive(Debug, Default)]
pub(crate) struct IdentityManifestSummary {
    pub alias: String,
    pub mailboxes: usize,
    pub pending_messages: usize,
    pub pending_bytes: u64,
}

/// One mailbox's pending (not yet checkpointed) UIDs, discovered by
/// `gather_pending`. Concurrency-independent -- turning this into actual
/// `Batch`es (which depends on the wizard-chosen `--concurrency`) is
/// `batches_from_pending`'s job, kept separate so the wizard can show the
/// manifest summary *before* asking for a concurrency value.
pub(crate) struct PendingMailbox {
    pub mailbox: String,
    pub mailbox_relpath: PathBuf,
    pub uids: Vec<u32>,
}

const CONNECT_RETRIES: usize = 3;
const RETRY_BACKOFF: Duration = Duration::from_secs(5);

/// Retries `f` up to `attempts` times, sleeping `backoff * attempt_number`
/// between tries (linear: `backoff`, `2*backoff`, ...) before giving up --
/// absorbs a transient provider-side throttle (e.g. a burst of
/// `concurrency` workers all connecting within the same instant at job
/// start) instead of failing on the first timeout. Generic over `f` so it's
/// testable without any real I/O (see the unit tests below).
async fn retry_with_backoff<T, F, Fut>(
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
/// one-shot per-identity connection and by each persistent worker's
/// initial connect/reconnect (`run_worker`, below).
async fn connect_with_retry(ctx: &IdentityContext) -> Result<ImapSession, String> {
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

/// Connects to `ctx`'s identity, lists its mailboxes, and for each one:
/// resets on a `UIDVALIDITY` change (ADR-0005 precedent, applied to the new
/// checkpoint per the ADR-0021 addendum), computes pending UIDs (server
/// minus already-checkpointed), and resolves their sizes -- reusing a
/// persisted `.manifest` where it already covers every pending UID (no
/// extra IMAP round-trip beyond the `UID SEARCH ALL` already needed for the
/// staleness/new-mail check), pulling fresh sizes otherwise (ADR-0021
/// §3/§4). Returns every mailbox's pending UIDs plus a summary for the
/// wizard to display, and persists the freshly rebuilt manifest.
pub(crate) async fn gather_pending(
    ctx: &IdentityContext,
) -> Result<(Vec<PendingMailbox>, IdentityManifestSummary), String> {
    let mut session = connect_with_retry(ctx).await?;

    let names: Vec<_> = session
        .list(None, Some("*"))
        .await
        .map_err(|err| format!("failed to list mailboxes: {err}"))?
        .try_collect()
        .await
        .map_err(|err| format!("failed to list mailboxes: {err}"))?;
    let mailboxes: Vec<(String, Option<String>)> = names
        .iter()
        .filter(|name| !name.attributes().contains(&NameAttribute::NoSelect))
        .map(|name| {
            (
                name.name().to_string(),
                name.delimiter().map(str::to_string),
            )
        })
        .collect();

    let checkpoint_entries = manifest::load_checkpoint(&ctx.staging_dir)?;
    let done = manifest::done_uids(&checkpoint_entries);
    let persisted_manifest = manifest::load_manifest(&ctx.staging_dir)?;
    let mut persisted_sizes: HashMap<(String, u32), u64> = HashMap::new();
    for entry in &persisted_manifest {
        persisted_sizes.insert((entry.mailbox.clone(), entry.uid), entry.size);
    }

    let mut pending_mailboxes = Vec::new();
    let mut fresh_manifest = Vec::new();
    let mut summary = IdentityManifestSummary {
        alias: ctx.identity.alias.clone(),
        ..Default::default()
    };

    for (mailbox_name, delimiter) in &mailboxes {
        let mailbox_relpath = sink::sanitize_mailbox_path(mailbox_name, delimiter.as_deref());
        let mailbox_dir = ctx.staging_dir.join(&mailbox_relpath);
        fs::create_dir_all(&mailbox_dir)
            .map_err(|err| format!("failed to create {}: {err}", mailbox_dir.display()))?;

        let mailbox_response = session
            .examine(mailbox_name)
            .await
            .map_err(|err| format!("failed to open '{mailbox_name}' read-only: {err}"))?;
        let current_validity = mailbox_response.uid_validity.unwrap_or(0);
        if sink::is_stale(sink::read_uidvalidity(&mailbox_dir), current_validity) {
            sink::clear_eml_files(&mailbox_dir)?;
            manifest::clear_checkpoint_for_mailbox(&ctx.staging_dir, mailbox_name)?;
        }
        sink::write_uidvalidity(&mailbox_dir, current_validity)?;

        let server_uids: HashSet<u32> = session
            .uid_search("ALL")
            .await
            .map_err(|err| format!("failed to search '{mailbox_name}': {err}"))?;
        let done_here: HashSet<u32> = done
            .iter()
            .filter(|(mailbox, _)| mailbox == mailbox_name)
            .map(|(_, uid)| *uid)
            .collect();
        let pending_uids = sink::missing_uids(&server_uids, &done_here);
        if pending_uids.is_empty() {
            continue;
        }

        let all_sizes_known = pending_uids
            .iter()
            .all(|uid| persisted_sizes.contains_key(&(mailbox_name.clone(), *uid)));
        let mailbox_manifest: Vec<ManifestEntry> = if all_sizes_known {
            pending_uids
                .iter()
                .map(|uid| ManifestEntry {
                    mailbox: mailbox_name.clone(),
                    uid: *uid,
                    size: persisted_sizes[&(mailbox_name.clone(), *uid)],
                })
                .collect()
        } else {
            manifest::pull_manifest(&mut session, mailbox_name, &pending_uids).await?
        };

        summary.mailboxes += 1;
        summary.pending_messages += mailbox_manifest.len();
        summary.pending_bytes += mailbox_manifest.iter().map(|entry| entry.size).sum::<u64>();
        fresh_manifest.extend(mailbox_manifest);

        pending_mailboxes.push(PendingMailbox {
            mailbox: mailbox_name.clone(),
            mailbox_relpath,
            uids: pending_uids,
        });
    }

    session
        .logout()
        .await
        .map_err(|err| format!("logout failed: {err}"))?;

    manifest::save_manifest(&ctx.staging_dir, &fresh_manifest)?;

    Ok((pending_mailboxes, summary))
}

/// Splits every mailbox's pending UIDs into `Batch`es at the chosen
/// `concurrency` (ADR-0021 §6) -- kept separate from `gather_pending` so
/// that call's IMAP round-trips don't need to happen again once the wizard
/// knows what concurrency to batch at.
pub(crate) fn batches_from_pending(pending: &[PendingMailbox], concurrency: usize) -> Vec<Batch> {
    pending
        .iter()
        .flat_map(|mailbox| {
            manifest::split_into_batches(&mailbox.uids, concurrency)
                .into_iter()
                .map(|uids| Batch {
                    mailbox: mailbox.mailbox.clone(),
                    mailbox_relpath: mailbox.mailbox_relpath.clone(),
                    uids,
                })
        })
        .collect()
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
/// it pulls belong to that identity (ADR-0021 §6 addendum) -- the direct
/// fix for the connection-churn bug: a job with `concurrency` workers now
/// opens on the order of `concurrency` connections total over its whole
/// run, not one per batch. A connect/re-`EXAMINE` failure (even after
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

    let mut outcome = BatchOutcome::default();
    for uid in &batch.uids {
        let eml_path = mailbox_dir.join(format!("{uid}.eml"));
        let transformed =
            transform::transform_one(&ctx.identity, &eml_path, &ctx.staging_dir, &ctx.staging_dir)?;
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
/// its own. Ported essentially verbatim from the removed `email::sync`
/// (ADR-0019).
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
/// host platform's path separator. Ported verbatim from `email::sync`.
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
    let files = crate::dataops::transform::collect_files(identity_dir)?;

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
/// calls `gather_pending` itself to build the pre-run summary, so this
/// reuses that result instead of re-connecting to IMAP. Splits every
/// mailbox's pending UIDs into batches at `concurrency` (ADR-0021 §6), then
/// fetches+transforms them all concurrently through one worker pool
/// spanning every identity's every mailbox (this is what fully fixes
/// Context problem 1, the concurrency-capped-by-mailbox-count bug), then
/// runs each identity's dedup pass and (if `remote` is given) upload phase
/// sequentially, one identity after another.
pub(crate) async fn run_email_sync_job(
    identities: Vec<IdentityContext>,
    pending_by_identity: Vec<Vec<PendingMailbox>>,
    concurrency: usize,
    remote: Option<(&BucketConfig, &str)>,
) -> Result<JobSummary, String> {
    let mut all_batches: VecDeque<(usize, Batch)> = VecDeque::new();
    for (index, pending) in pending_by_identity.iter().enumerate() {
        let batches = batches_from_pending(pending, concurrency);
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
        let mut message_index =
            ContentIndex::load(&ctx.staging_dir, transform::MESSAGE_HASHES_FILE)?;
        let mut attachment_index =
            ContentIndex::load(&ctx.staging_dir, transform::ATTACHMENT_HASHES_FILE)?;
        let dedup_summary = run_dedup_pass(
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

/// Outcome of a completed dedup pass, for the caller's summary line.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct DedupSummary {
    pub merged_messages: usize,
    pub deduped_attachments: usize,
}

/// The single-threaded, post-transform dedup pass (ADR-0021 §7/§10): places
/// every checkpointed message and attachment at its final, canonical
/// location under `identity_dir` (the flat, identity-rooted tree, ADR-0006),
/// merging real content-hash duplicates instead of writing them twice, and
/// closing the `unique_path` TOCTOU race (ADR-0021 addendum) by being the
/// only caller of `unique_path` against that shared tree.
///
/// `entries` is sorted by `(mailbox, uid)` ascending first -- batches
/// complete out of order under concurrency, so checkpoint append order
/// isn't deterministic across runs, and reproducible canonical-occurrence
/// selection requires a stable processing order.
///
/// Runs in two passes over the sorted entries: messages first (so every
/// canonical message's final path is settled), then attachments of
/// canonical (non-merged) messages only -- a merged duplicate's own
/// attachments are redundant by construction (identical message hash means
/// identical raw bytes, so the canonical message's own attachments already
/// cover them) and are simply deleted alongside its staged `.md`.
pub(crate) fn run_dedup_pass(
    identity_dir: &Path,
    staging_dir: &Path,
    entries: &mut [CheckpointEntry],
    message_index: &mut ContentIndex,
    attachment_index: &mut ContentIndex,
) -> Result<DedupSummary, String> {
    entries.sort_by(|a, b| (&a.mailbox, a.uid).cmp(&(&b.mailbox, b.uid)));

    let mut summary = DedupSummary::default();
    let mut placed_md_paths = vec![None; entries.len()];

    for (index, entry) in entries.iter().enumerate() {
        if !staging_dir.join(&entry.md_staged_relpath).exists() {
            // Already fully handled by a prior dedup pass run (placed as
            // canonical, or merged as a duplicate -- either way its staged
            // `.md` was moved or deleted). `message_index` only records the
            // current canonical path per hash, not which specific entry
            // produced it, so re-deriving "was this entry canonical or a
            // duplicate" from the index alone isn't reliable -- checking
            // `message_index.check()` again here could wrongly treat an
            // already-canonical entry as a self-duplicate of itself.
            // Skipping whenever the staged file is already gone sidesteps
            // that ambiguity entirely and is always safe: there is nothing
            // left for this entry to do.
            continue;
        }

        match message_index.check(&entry.message_hash) {
            Some(canonical_relpath) => {
                let canonical_path = identity_dir.join(canonical_relpath);
                match dedup::amend_frontmatter_for_duplicate(
                    &canonical_path,
                    &entry.mailbox_tag,
                    entry.uid,
                ) {
                    Ok(_) => {
                        summary.merged_messages += 1;
                        remove_staged_files(staging_dir, entry);
                    }
                    Err(err) => {
                        eprintln!(
                            "Warning: canonical file for duplicate {} is missing or malformed: {err}, treating as canonical instead",
                            entry.md_staged_relpath
                        );
                        placed_md_paths[index] = Some(place_canonical_message(
                            identity_dir,
                            staging_dir,
                            entry,
                            message_index,
                        )?);
                    }
                }
            }
            None => {
                placed_md_paths[index] = Some(place_canonical_message(
                    identity_dir,
                    staging_dir,
                    entry,
                    message_index,
                )?);
            }
        }
    }

    for (index, entry) in entries.iter().enumerate() {
        let Some(md_path) = &placed_md_paths[index] else {
            continue;
        };
        for (hash, staged_relpath) in &entry.attachments {
            let staged_path = staging_dir.join(staged_relpath);
            if !staged_path.exists() {
                // Same idempotency guard as the message-level pass above.
                continue;
            }
            match attachment_index.check(hash) {
                Some(canonical_relpath) => {
                    summary.deduped_attachments += 1;
                    let _ = fs::remove_file(&staged_path);
                    if dedup::rewrite_attachment_reference(
                        md_path,
                        staged_relpath,
                        canonical_relpath,
                    )? {
                        // Rewritten to point at the pre-existing canonical
                        // attachment.
                    }
                }
                None => {
                    let file_name = Path::new(staged_relpath)
                        .file_name()
                        .map(|name| name.to_string_lossy().into_owned())
                        .unwrap_or_else(|| staged_relpath.clone());
                    let attachments_dir = identity_dir.join("attachments");
                    fs::create_dir_all(&attachments_dir).map_err(|err| {
                        format!("failed to create {}: {err}", attachments_dir.display())
                    })?;
                    let final_path = unique_path(&attachments_dir.join(&file_name));
                    fs::rename(&staged_path, &final_path).map_err(|err| {
                        format!(
                            "failed to move {} to {}: {err}",
                            staged_path.display(),
                            final_path.display()
                        )
                    })?;
                    let final_relpath = format!(
                        "attachments/{}",
                        final_path.file_name().unwrap().to_string_lossy()
                    );
                    attachment_index.commit(staging_dir, hash, &final_relpath)?;
                    if final_relpath != *staged_relpath {
                        dedup::rewrite_attachment_reference(
                            md_path,
                            staged_relpath,
                            &final_relpath,
                        )?;
                    }
                }
            }
        }
    }

    Ok(summary)
}

/// Places a canonical (non-duplicate) message's staged `.md` at its final
/// location under `identity_dir`, resolving any genuine filename collision
/// via `unique_path`, and commits its hash to `message_index`. Returns the
/// final path, needed by the attachment-placement pass above to target
/// `rewrite_attachment_reference` calls at the right file.
fn place_canonical_message(
    identity_dir: &Path,
    staging_dir: &Path,
    entry: &CheckpointEntry,
    message_index: &mut ContentIndex,
) -> Result<PathBuf, String> {
    fs::create_dir_all(identity_dir)
        .map_err(|err| format!("failed to create {}: {err}", identity_dir.display()))?;
    let final_path = unique_path(&identity_dir.join(&entry.desired_md_name));
    let staged_path = staging_dir.join(&entry.md_staged_relpath);
    fs::rename(&staged_path, &final_path).map_err(|err| {
        format!(
            "failed to move {} to {}: {err}",
            staged_path.display(),
            final_path.display()
        )
    })?;
    let final_relpath = final_path
        .file_name()
        .unwrap()
        .to_string_lossy()
        .into_owned();
    message_index.commit(staging_dir, &entry.message_hash, &final_relpath)?;
    Ok(final_path)
}

/// Deletes a merged duplicate's staged `.md` and every staged attachment it
/// referenced -- all redundant once the entry is merged into an existing
/// canonical file.
fn remove_staged_files(staging_dir: &Path, entry: &CheckpointEntry) {
    let _ = fs::remove_file(staging_dir.join(&entry.md_staged_relpath));
    for (_, relpath) in &entry.attachments {
        let _ = fs::remove_file(staging_dir.join(relpath));
    }
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

    fn entry(
        mailbox: &str,
        uid: u32,
        message_hash: &str,
        desired_md_name: &str,
        attachments: Vec<(&str, &str)>,
    ) -> CheckpointEntry {
        CheckpointEntry {
            mailbox: mailbox.to_string(),
            uid,
            message_hash: message_hash.to_string(),
            md_staged_relpath: format!("transformed/{mailbox}/{uid}.md"),
            desired_md_name: desired_md_name.to_string(),
            mailbox_tag: format!("mailbox/{}", mailbox.to_lowercase()),
            attachments: attachments
                .into_iter()
                .map(|(hash, relpath)| (hash.to_string(), relpath.to_string()))
                .collect(),
        }
    }

    fn stage_message(staging_dir: &Path, entry: &CheckpointEntry, body: &str) {
        let path = staging_dir.join(&entry.md_staged_relpath);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, body).unwrap();
    }

    fn stage_attachment(staging_dir: &Path, relpath: &str, contents: &[u8]) {
        let path = staging_dir.join(relpath);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, contents).unwrap();
    }

    const FIXTURE_BODY: &str = "---\nfrom: \"a\"\ntags:\n  - mailbox/inbox\n---\nbody";

    #[test]
    fn run_dedup_pass_places_a_lone_canonical_message() {
        let staging = tempfile::tempdir().unwrap();
        let identity_dir = tempfile::tempdir().unwrap();
        let mut message_index =
            ContentIndex::load(staging.path(), crate::email::transform::MESSAGE_HASHES_FILE)
                .unwrap();
        let mut attachment_index = ContentIndex::load(
            staging.path(),
            crate::email::transform::ATTACHMENT_HASHES_FILE,
        )
        .unwrap();

        let e = entry("INBOX", 1, "hash-a", "2024-01-26-hello.md", vec![]);
        stage_message(staging.path(), &e, FIXTURE_BODY);
        let mut entries = vec![e];

        let summary = run_dedup_pass(
            identity_dir.path(),
            staging.path(),
            &mut entries,
            &mut message_index,
            &mut attachment_index,
        )
        .unwrap();

        assert_eq!(summary.merged_messages, 0);
        assert!(identity_dir.path().join("2024-01-26-hello.md").exists());
        assert!(!staging.path().join("transformed/INBOX/1.md").exists());
        assert_eq!(message_index.check("hash-a"), Some("2024-01-26-hello.md"));
    }

    #[test]
    fn run_dedup_pass_merges_two_messages_with_the_same_hash() {
        let staging = tempfile::tempdir().unwrap();
        let identity_dir = tempfile::tempdir().unwrap();
        let mut message_index =
            ContentIndex::load(staging.path(), crate::email::transform::MESSAGE_HASHES_FILE)
                .unwrap();
        let mut attachment_index = ContentIndex::load(
            staging.path(),
            crate::email::transform::ATTACHMENT_HASHES_FILE,
        )
        .unwrap();

        let first = entry("Archive", 2, "same-hash", "2024-01-26-hello.md", vec![]);
        let second = entry("INBOX", 1, "same-hash", "2024-01-26-hello.md", vec![]);
        stage_message(staging.path(), &first, FIXTURE_BODY);
        stage_message(staging.path(), &second, FIXTURE_BODY);
        let mut entries = vec![second, first];

        let summary = run_dedup_pass(
            identity_dir.path(),
            staging.path(),
            &mut entries,
            &mut message_index,
            &mut attachment_index,
        )
        .unwrap();

        // Sorted order is (Archive, 2) then (INBOX, 1) -- Archive comes
        // first alphabetically, so it becomes canonical.
        assert_eq!(summary.merged_messages, 1);
        let md_files: Vec<_> = fs::read_dir(identity_dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.path().extension().and_then(|x| x.to_str()) == Some("md"))
            .collect();
        assert_eq!(md_files.len(), 1);
        let contents = fs::read_to_string(md_files[0].path()).unwrap();
        assert!(contents.contains("also-in:"));
        assert!(contents.contains("mailbox/inbox#1"));
    }

    #[test]
    fn run_dedup_pass_dedupes_cross_message_attachment_and_rewrites_reference() {
        let staging = tempfile::tempdir().unwrap();
        let identity_dir = tempfile::tempdir().unwrap();
        let mut message_index =
            ContentIndex::load(staging.path(), crate::email::transform::MESSAGE_HASHES_FILE)
                .unwrap();
        let mut attachment_index = ContentIndex::load(
            staging.path(),
            crate::email::transform::ATTACHMENT_HASHES_FILE,
        )
        .unwrap();

        let first = entry(
            "Archive",
            1,
            "hash-1",
            "2024-01-26-first.md",
            vec![("attach-hash", "attachments/a.pdf")],
        );
        let second = entry(
            "INBOX",
            2,
            "hash-2",
            "2024-01-27-second.md",
            vec![("attach-hash", "attachments/a.pdf")],
        );
        let body_with_attachment = "---\nfrom: \"a\"\ntags:\n  - mailbox/inbox\nattachments:\n  - attachments/a.pdf\n---\nbody";
        stage_message(staging.path(), &first, body_with_attachment);
        stage_message(staging.path(), &second, body_with_attachment);
        stage_attachment(staging.path(), "attachments/a.pdf", b"content-1");
        // Different staged path (per-uid staging tree keeps them apart) but
        // identical content hash.
        let second_attachment_relpath = "transformed/INBOX/2/attachments/a.pdf";
        stage_attachment(staging.path(), second_attachment_relpath, b"content-1");
        let mut second_with_real_path = second;
        second_with_real_path.attachments = vec![(
            "attach-hash".to_string(),
            second_attachment_relpath.to_string(),
        )];

        let mut entries = vec![first, second_with_real_path];

        let summary = run_dedup_pass(
            identity_dir.path(),
            staging.path(),
            &mut entries,
            &mut message_index,
            &mut attachment_index,
        )
        .unwrap();

        assert_eq!(summary.deduped_attachments, 1);
        let attachments_dir = identity_dir.path().join("attachments");
        assert_eq!(fs::read_dir(&attachments_dir).unwrap().count(), 1);

        let second_md = identity_dir.path().join("2024-01-27-second.md");
        let contents = fs::read_to_string(&second_md).unwrap();
        assert!(contents.contains("attachments:\n  - attachments/a.pdf"));
    }

    #[test]
    fn run_dedup_pass_is_idempotent_when_rerun_on_the_same_entries() {
        // A real caller (job::email_sync's orchestrator) may pass the same
        // identity's full checkpoint history to every job run, not just
        // newly-added entries -- so a second call with entries already
        // fully placed by a prior call must be a safe no-op, not corrupt
        // the already-canonical file (e.g. by treating it as a duplicate of
        // itself, since `message_index.check()` alone can't distinguish
        // "this entry is the canonical one" from "this is a fresh
        // duplicate" once the hash is committed).
        let staging = tempfile::tempdir().unwrap();
        let identity_dir = tempfile::tempdir().unwrap();
        let mut message_index =
            ContentIndex::load(staging.path(), crate::email::transform::MESSAGE_HASHES_FILE)
                .unwrap();
        let mut attachment_index = ContentIndex::load(
            staging.path(),
            crate::email::transform::ATTACHMENT_HASHES_FILE,
        )
        .unwrap();

        let e = entry("INBOX", 1, "hash-a", "2024-01-26-hello.md", vec![]);
        stage_message(staging.path(), &e, FIXTURE_BODY);
        let mut entries = vec![e];

        run_dedup_pass(
            identity_dir.path(),
            staging.path(),
            &mut entries,
            &mut message_index,
            &mut attachment_index,
        )
        .unwrap();
        let after_first_run =
            fs::read_to_string(identity_dir.path().join("2024-01-26-hello.md")).unwrap();

        let summary = run_dedup_pass(
            identity_dir.path(),
            staging.path(),
            &mut entries,
            &mut message_index,
            &mut attachment_index,
        )
        .unwrap();

        assert_eq!(summary, DedupSummary::default());
        assert_eq!(
            fs::read_to_string(identity_dir.path().join("2024-01-26-hello.md")).unwrap(),
            after_first_run,
            "re-running the pass must not append a spurious self-referential also-in entry"
        );
    }
}
