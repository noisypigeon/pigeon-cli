//! The fetch/push pipeline (CLI-0146 §5).
//!
//! Shaped like `email_pull::worker`'s interleaved queue plus per-identity
//! connection semaphore (CLI-0021 §6 addendum, CLI-0071), with three
//! substantive differences: every fetch reserves its rate window first
//! (§4), every message is pushed and then deleted immediately rather than
//! accumulating for a later upload phase (§5), and both legs park on a
//! `RecoveryGate` instead of burning their retry budget against an
//! endpoint that is refusing work (§4).

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use futures::StreamExt;
use indicatif::MultiProgress;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tracing::Instrument;

use crate::commands::job::local;
use crate::commands::keyring::email::identity;
use crate::commands::keyring::email::imap_client::{self, ImapSession};
use crate::core::retry::retry_with_backoff;

use super::gate::{GateDecision, RecoveryGate, is_server_overloaded_error};
use super::manifest::{self, Batch, CheckpointEntry};
use super::sink;
use super::{ImportContext, ImportJob, PendingMailbox, attach, push};

const JOB: &str = "email-import";

/// One queued unit of work: which identity it belongs to, the batch
/// itself, and how many requeue attempts it has left.
type QueueItem = (usize, Batch, u8);

/// The shared work queue every worker pops from and requeues onto.
type WorkQueue = Arc<Mutex<VecDeque<QueueItem>>>;

/// Connect budget. `retry_with_backoff`'s schedule is linear, so `(4, 30s)`
/// attempts at t=0/+30s/+90s/+180s -- three minutes of patience, the same
/// schedule `CopyTo::retry()` uses since CLI-0144. (That function's doc
/// comment calls the same schedule "about five minutes", which sums the
/// attempt timestamps rather than the sleeps; the wall clock is 180s. Noted
/// here so the two are not read as different budgets -- correcting the
/// wording there is CLI-0144's to do, not this ADR's.)
///
/// Either way it is an order of magnitude more patient than the sibling
/// email jobs' `(3, 5s)` -- fifteen seconds, far too short for the real
/// 74-second outage CLI-0144 documented, and structurally futile against a
/// negative-TTL-cached DNS answer.
const CONNECT_RETRIES: usize = 4;
const CONNECT_BACKOFF: Duration = Duration::from_secs(30);

/// Batch requeue budget: 30/60/90s, scaled up from the sibling jobs' `2s`
/// for the same reason as the connect budget.
const BATCH_RETRIES: u8 = 3;
const BATCH_RETRY_BACKOFF: Duration = Duration::from_secs(30);

/// IMAP recovery: six rounds at 60s linear, about 21 minutes of patience
/// before the gate latches.
const IMAP_RECOVERY_ROUNDS: usize = 6;
const IMAP_RECOVERY_BACKOFF: Duration = Duration::from_secs(60);

/// Push recovery: four rounds at 30s, about five minutes, on top of
/// `CopyTo`'s own five-minute per-object retry budget.
const PUSH_RECOVERY_ROUNDS: usize = 4;
const PUSH_RECOVERY_BACKOFF: Duration = Duration::from_secs(30);

/// Connects and logs in to `ctx`'s identity, retrying on failure.
///
/// A deliberate near-duplicate of the sibling jobs' `connect_with_retry`
/// (both of which live in private `mod worker`s and are unreachable from
/// here anyway), differing in its budget -- which is the whole point
/// (CLI-0146 §2, §4).
#[tracing::instrument(skip_all, fields(identity = %ctx.identity.alias))]
pub(crate) async fn connect_with_retry(ctx: &ImportContext) -> Result<ImapSession, String> {
    retry_with_backoff(CONNECT_RETRIES, CONNECT_BACKOFF, || {
        imap_client::connect_and_login(
            &ctx.identity.host,
            ctx.identity.port,
            &ctx.identity.email,
            &ctx.secret,
            ctx.identity.security(),
            ctx.identity.provider.accepts_invalid_certs(),
        )
    })
    .await
}

/// A `failed` count broken down by cause. Every variant is a message that
/// did **not** reach the destination, so these sum to the flat `failed`
/// counter (CLI-0033).
#[derive(Debug, Default)]
pub(crate) struct FailureBreakdown {
    pub connect: usize,
    pub examine: usize,
    pub fetch: usize,
    pub missing_file: usize,
    pub push: usize,
}

impl FailureBreakdown {
    fn merge(&mut self, other: &FailureBreakdown) {
        self.connect += other.connect;
        self.examine += other.examine;
        self.fetch += other.fetch;
        self.missing_file += other.missing_file;
        self.push += other.push;
    }
}

#[derive(Default)]
struct BatchOutcome {
    delivered: usize,
    failed: usize,
    objects_pushed: usize,
    attachments_pushed: usize,
    attachment_extraction_failed: usize,
    gate_give_ups: usize,
    failure_breakdown: FailureBreakdown,
}

/// Outcome of a full run, across every selected identity.
#[derive(Debug, Default)]
pub(crate) struct JobSummary {
    /// Messages whose `.eml` and every attachment reached the destination.
    pub delivered: usize,
    pub failed: usize,
    pub failure_breakdown: FailureBreakdown,
    /// Total objects pushed, i.e. messages plus attachments.
    pub objects_pushed: usize,
    pub attachments_pushed: usize,
    /// Informational, deliberately **not** part of `failed`: the raw `.eml`
    /// is this job's primary deliverable and already contains every
    /// attachment inline, so an unparseable message is still delivered
    /// (CLI-0081's precedent, same reasoning).
    pub attachment_extraction_failed: usize,
    /// How many items failed because a `RecoveryGate` had latched.
    /// Informational and overlapping with `failed`, so not part of the
    /// breakdown sum -- it answers "was the run beaten by an outage?"
    pub gate_give_ups: usize,
}

/// A worker's currently-open IMAP session, if any.
struct WorkerConnection {
    identity_index: usize,
    mailbox: String,
    session: ImapSession,
    _permit: OwnedSemaphorePermit,
}

/// After a batch fails, either sleeps briefly and returns a requeue-able
/// item with one fewer attempt remaining, or `None` once retries are
/// exhausted.
async fn requeue_or_none(
    identity_index: usize,
    batch: Batch,
    attempts_remaining: u8,
    backoff: Duration,
) -> Option<QueueItem> {
    if attempts_remaining == 0 {
        return None;
    }
    tokio::time::sleep(backoff).await;
    Some((identity_index, batch, attempts_remaining - 1))
}

/// Everything a worker needs that is shared across all of them.
struct Shared {
    contexts: Arc<Vec<ImportContext>>,
    identity_semaphores: Arc<Vec<Arc<Semaphore>>>,
    pacer: Arc<super::pace::Pacer>,
    imap_gate: Arc<RecoveryGate>,
    push_gate: Arc<RecoveryGate>,
    push_semaphore: Arc<Semaphore>,
    destination: Arc<String>,
    push_log_dir: Arc<PathBuf>,
    push_concurrency: usize,
}

/// Parks on the IMAP gate when `err` says the server is refusing work.
/// Returns `false` once the gate has latched, meaning the caller should
/// fail the batch now rather than requeue it.
async fn wait_out_imap_outage(gate: &RecoveryGate, subject: &str, err: &str) -> bool {
    if !is_server_overloaded_error(err) {
        return true;
    }
    match gate.wait_for_recovery(subject, err).await {
        GateDecision::Retry => true,
        GateDecision::GiveUp => false,
    }
}

/// Pulls `(identity_index, Batch, attempts_remaining)` items from `queue`
/// until it drains, holding one IMAP session per identity for as long as
/// consecutive batches belong to that identity.
async fn run_worker(
    queue: WorkQueue,
    shared: Arc<Shared>,
    multi_progress: MultiProgress,
) -> BatchOutcome {
    let mut connection: Option<WorkerConnection> = None;
    let mut outcome = BatchOutcome::default();

    loop {
        let next = { queue.lock().unwrap().pop_front() };
        let Some((identity_index, batch, attempts_remaining)) = next else {
            break;
        };
        let ctx = &shared.contexts[identity_index];

        if connection
            .as_ref()
            .is_none_or(|conn| conn.identity_index != identity_index)
        {
            if let Some(mut conn) = connection.take() {
                let _ = conn.session.logout().await;
            }
            let permit = shared.identity_semaphores[identity_index]
                .clone()
                .acquire_owned()
                .await
                .expect("identity semaphores are never closed");
            match connect_with_retry(ctx).await {
                Ok(session) => {
                    crate::observability::metrics::record_phase(JOB, "connect", "ok", None);
                    connection = Some(WorkerConnection {
                        identity_index,
                        mailbox: String::new(),
                        session,
                        _permit: permit,
                    });
                }
                Err(err) => {
                    drop(permit);
                    let _ = multi_progress.println(format!("Error: {err}"));
                    let uid_count = batch.uids.len();
                    let mailbox = batch.mailbox.clone();
                    let may_retry =
                        wait_out_imap_outage(&shared.imap_gate, &ctx.identity.alias, &err).await;
                    if !may_retry {
                        outcome.gate_give_ups += uid_count;
                    }
                    let item = if may_retry {
                        requeue_or_none(
                            identity_index,
                            batch,
                            attempts_remaining,
                            BATCH_RETRY_BACKOFF,
                        )
                        .await
                    } else {
                        None
                    };
                    record_dropped_or_requeued(
                        &queue,
                        item,
                        ctx,
                        &mailbox,
                        uid_count,
                        BatchFailureStep::Connect,
                        &mut outcome,
                    );
                    continue;
                }
            }
        }

        let conn = connection.as_mut().expect("just connected");
        if conn.mailbox != batch.mailbox {
            match conn.session.examine(&batch.mailbox).await {
                Ok(_) => {
                    conn.mailbox = batch.mailbox.clone();
                    crate::observability::metrics::record_phase(JOB, "examine", "ok", None);
                }
                Err(err) => {
                    let _ = multi_progress.println(format!(
                        "Error: failed to open '{}' read-only: {err}",
                        batch.mailbox
                    ));
                    connection = None;
                    let err = err.to_string();
                    let uid_count = batch.uids.len();
                    let mailbox = batch.mailbox.clone();
                    let may_retry = wait_out_imap_outage(&shared.imap_gate, &mailbox, &err).await;
                    if !may_retry {
                        outcome.gate_give_ups += uid_count;
                    }
                    let item = if may_retry {
                        requeue_or_none(
                            identity_index,
                            batch,
                            attempts_remaining,
                            BATCH_RETRY_BACKOFF,
                        )
                        .await
                    } else {
                        None
                    };
                    record_dropped_or_requeued(
                        &queue,
                        item,
                        ctx,
                        &mailbox,
                        uid_count,
                        BatchFailureStep::Examine,
                        &mut outcome,
                    );
                    continue;
                }
            }
        }

        let conn = connection.as_mut().expect("connected above");
        match process_batch_on_session(ctx, &batch, &mut conn.session, &shared, &multi_progress)
            .await
        {
            Ok(batch_outcome) => {
                outcome.delivered += batch_outcome.delivered;
                outcome.failed += batch_outcome.failed;
                outcome.objects_pushed += batch_outcome.objects_pushed;
                outcome.attachments_pushed += batch_outcome.attachments_pushed;
                outcome.attachment_extraction_failed += batch_outcome.attachment_extraction_failed;
                outcome.gate_give_ups += batch_outcome.gate_give_ups;
                outcome
                    .failure_breakdown
                    .merge(&batch_outcome.failure_breakdown);
            }
            Err(err) => {
                let _ = multi_progress.println(format!("Error: {err}"));
                connection = None;
                let uid_count = batch.uids.len();
                let mailbox = batch.mailbox.clone();
                let may_retry = wait_out_imap_outage(&shared.imap_gate, &mailbox, &err).await;
                if !may_retry {
                    outcome.gate_give_ups += uid_count;
                }
                let item = if may_retry {
                    requeue_or_none(
                        identity_index,
                        batch,
                        attempts_remaining,
                        BATCH_RETRY_BACKOFF,
                    )
                    .await
                } else {
                    None
                };
                record_dropped_or_requeued(
                    &queue,
                    item,
                    ctx,
                    &mailbox,
                    uid_count,
                    BatchFailureStep::Fetch,
                    &mut outcome,
                );
            }
        }
    }

    if let Some(mut conn) = connection {
        let _ = conn.session.logout().await;
    }

    outcome
}

/// Either pushes a requeue-able item back onto the queue with a warning, or
/// records its UIDs as failed in the given category. Factored out because
/// the three failure sites in `run_worker` differ only in their category.
/// `uid_count` and `mailbox` are passed in rather than read off `item`:
/// `requeue_or_none` consumes the `Batch` to build it, so on the drop path
/// there is no batch left to count, and a failure has to be charged for
/// every message it lost rather than once.
fn record_dropped_or_requeued(
    queue: &WorkQueue,
    item: Option<QueueItem>,
    ctx: &ImportContext,
    mailbox: &str,
    uid_count: usize,
    step: BatchFailureStep,
    outcome: &mut BatchOutcome,
) {
    let step_name = step.name();
    match item {
        Some(item) => {
            tracing::warn!(
                identity = %ctx.identity.alias,
                mailbox,
                step = step_name,
                attempts_remaining = item.2,
                "requeueing batch"
            );
            queue.lock().unwrap().push_back(item);
        }
        None => {
            tracing::error!(
                identity = %ctx.identity.alias,
                mailbox,
                step = step_name,
                uid_count,
                "batch retries exhausted, dropping"
            );
            outcome.failed += uid_count;
            *step.category(&mut outcome.failure_breakdown) += uid_count;
            crate::observability::metrics::record_phase_count(
                JOB,
                step_name,
                "failed",
                uid_count as u64,
                None,
            );
        }
    }
}

/// Which of `run_worker`'s three batch-level failure sites a drop belongs
/// to. An enum rather than a `&'static str` plus a `&mut usize` so the
/// metric label and the `FailureBreakdown` field can never drift apart --
/// the two always come from the same variant.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BatchFailureStep {
    Connect,
    Examine,
    Fetch,
}

impl BatchFailureStep {
    fn name(self) -> &'static str {
        match self {
            BatchFailureStep::Connect => "connect",
            BatchFailureStep::Examine => "examine",
            BatchFailureStep::Fetch => "fetch",
        }
    }

    fn category(self, breakdown: &mut FailureBreakdown) -> &mut usize {
        match self {
            BatchFailureStep::Connect => &mut breakdown.connect,
            BatchFailureStep::Examine => &mut breakdown.examine,
            BatchFailureStep::Fetch => &mut breakdown.fetch,
        }
    }
}

/// Reserves this batch's rate window, fetches it, then delivers each
/// message individually.
///
/// An `Err` here means the whole batch failed (a fetch-level problem), and
/// the caller requeues it. A per-message problem is recorded in the
/// returned outcome and never affects its siblings.
#[tracing::instrument(
    skip_all,
    fields(identity = %ctx.identity.alias, mailbox = %batch.mailbox, uid_count = batch.uids.len())
)]
async fn process_batch_on_session(
    ctx: &ImportContext,
    batch: &Batch,
    session: &mut ImapSession,
    shared: &Arc<Shared>,
    multi_progress: &MultiProgress,
) -> Result<BatchOutcome, String> {
    // The rate reservation, before any request leaves the process.
    shared.pacer.acquire(batch.uids.len()).await;

    // Local disk is bounded to the in-flight window by the GC below, so
    // this should never trip -- but a shared volume filling underneath the
    // run would otherwise surface as a confusing write failure per message.
    local::check_disk_space(&ctx.scratch_dir, 0)?;

    let mailbox_dir = ctx.scratch_dir.join(&batch.mailbox_relpath);
    std::fs::create_dir_all(&mailbox_dir)
        .map_err(|err| format!("failed to create {}: {err}", mailbox_dir.display()))?;

    sink::fetch_uids(
        session,
        &batch.mailbox,
        &mailbox_dir,
        &batch.uids,
        multi_progress,
    )
    .await?;

    let identity_prefix = identity::sanitize_segment(&ctx.identity.email);
    let mailbox_key = relative_key(&batch.mailbox_relpath);

    let outcomes = futures::stream::iter(batch.uids.iter().copied())
        .map(|uid| {
            let mailbox_dir = mailbox_dir.clone();
            let identity_prefix = identity_prefix.clone();
            let mailbox_key = mailbox_key.clone();
            async move {
                let _permit = shared
                    .push_semaphore
                    .clone()
                    .acquire_owned()
                    .await
                    .expect("the push semaphore is never closed");
                deliver_one(
                    ctx,
                    batch,
                    uid,
                    &mailbox_dir,
                    &identity_prefix,
                    &mailbox_key,
                    shared,
                    multi_progress,
                )
                .await
            }
        })
        .buffer_unordered(shared.push_concurrency.max(1))
        .collect::<Vec<BatchOutcome>>()
        .await;

    let mut outcome = BatchOutcome::default();
    for one in &outcomes {
        outcome.delivered += one.delivered;
        outcome.failed += one.failed;
        outcome.objects_pushed += one.objects_pushed;
        outcome.attachments_pushed += one.attachments_pushed;
        outcome.attachment_extraction_failed += one.attachment_extraction_failed;
        outcome.gate_give_ups += one.gate_give_ups;
        outcome.failure_breakdown.merge(&one.failure_breakdown);
    }
    Ok(outcome)
}

/// Delivers one message: unpack its attachments, push the `.eml` and each
/// attachment, then **always** delete the local copies.
///
/// The unconditional delete is this job's deliberate divergence from
/// CLI-0124's retain-on-failure rule, and is strictly safer than it: a
/// message whose push failed is still on the IMAP server, undamaged, where
/// a rerun refetches it anyway -- the same "non-destructive, still at the
/// source" reasoning CLI-0143 used. `transform` retains a failure because a
/// transcode result is not trivially reproducible; nothing here is. The
/// consequence is that local disk is bounded to the in-flight window
/// unconditionally, making CLI-0143's retention pathology structurally
/// impossible rather than merely unlikely.
#[allow(clippy::too_many_arguments)]
async fn deliver_one(
    ctx: &ImportContext,
    batch: &Batch,
    uid: u32,
    mailbox_dir: &Path,
    identity_prefix: &str,
    mailbox_key: &str,
    shared: &Arc<Shared>,
    multi_progress: &MultiProgress,
) -> BatchOutcome {
    let mut outcome = BatchOutcome::default();
    let eml_path = mailbox_dir.join(format!("{uid}.eml"));
    let uid_dir = mailbox_dir.join(uid.to_string());

    if !eml_path.exists() {
        // `fetch_uids` warns and skips a FETCH response missing its uid or
        // body; this is where that becomes a counted failure.
        tracing::warn!(
            uid,
            mailbox = %batch.mailbox,
            step = "fetch",
            "eml missing after fetch, counted as failed"
        );
        outcome.failed += 1;
        outcome.failure_breakdown.missing_file += 1;
        crate::observability::metrics::record_phase(JOB, "missing_file", "failed", None);
        return outcome;
    }

    let attachments_dir = uid_dir.join("attachments");
    let attachments = multi_progress
        .suspend(|| attach::extract_attachments(&eml_path, &attachments_dir, uid, &batch.mailbox));
    let attachments = match attachments {
        Ok(attachments) => {
            crate::observability::metrics::record_phase(JOB, "extract", "ok", None);
            attachments
        }
        Err(()) => {
            // Informational, not a failure: the raw `.eml` already carries
            // every attachment inline and is still delivered below.
            outcome.attachment_extraction_failed += 1;
            crate::observability::metrics::record_phase(JOB, "extract", "failed", None);
            Vec::new()
        }
    };

    let mut objects: Vec<(PathBuf, String)> = Vec::with_capacity(attachments.len() + 1);
    objects.push((
        eml_path.clone(),
        format!("{identity_prefix}/{mailbox_key}{uid}.eml"),
    ));
    for attachment in &attachments {
        objects.push((
            attachment.path.clone(),
            format!(
                "{identity_prefix}/{mailbox_key}{uid}/attachments/{}",
                attachment.name
            ),
        ));
    }

    let mut all_pushed = true;
    for (path, key) in &objects {
        match push_with_gate(path, key, shared).await {
            Ok(()) => {
                outcome.objects_pushed += 1;
                if path != &eml_path {
                    outcome.attachments_pushed += 1;
                }
            }
            Err(PushFailure { error, gated }) => {
                tracing::warn!(
                    uid,
                    mailbox = %batch.mailbox,
                    key = %key,
                    step = "push",
                    error = %error,
                    "failed to push object"
                );
                all_pushed = false;
                if gated {
                    outcome.gate_give_ups += 1;
                }
                break;
            }
        }
    }

    if all_pushed {
        // Only now is the UID durably delivered. Appending earlier would
        // let a rerun skip a message that never reached the destination.
        if let Err(err) = manifest::append_checkpoint(
            &ctx.staging_dir,
            &CheckpointEntry {
                mailbox: batch.mailbox.clone(),
                uid,
            },
        ) {
            tracing::warn!(uid, mailbox = %batch.mailbox, error = %err, "failed to checkpoint a delivered message");
        }
        outcome.delivered += 1;
        crate::observability::metrics::record_phase(JOB, "push", "ok", None);
    } else {
        outcome.failed += 1;
        outcome.failure_breakdown.push += 1;
        crate::observability::metrics::record_phase(JOB, "push", "failed", None);
    }

    // CLI-0146 §5: always, on success and on failure alike.
    remove_local_copy(&eml_path);
    remove_local_dir(&uid_dir);

    outcome
}

struct PushFailure {
    error: String,
    /// Whether the push gate latched, as opposed to an ordinary failure.
    gated: bool,
}

/// Pushes one object, parking on the push gate for as long as the
/// destination endpoint is unreachable.
async fn push_with_gate(path: &Path, key: &str, shared: &Arc<Shared>) -> Result<(), PushFailure> {
    loop {
        match push::push_one(path, &shared.destination, key, &shared.push_log_dir).await {
            Ok(()) => return Ok(()),
            Err(error) => {
                if !crate::core::rclone::is_endpoint_unreachable_error(&error) {
                    return Err(PushFailure {
                        error,
                        gated: false,
                    });
                }
                match shared.push_gate.wait_for_recovery(key, &error).await {
                    GateDecision::Retry => continue,
                    GateDecision::GiveUp => {
                        return Err(PushFailure { error, gated: true });
                    }
                }
            }
        }
    }
}

/// Best-effort delete. A failure is logged and never flips a successful
/// delivery into a failure (CLI-0124's discipline).
fn remove_local_copy(path: &Path) {
    if let Err(err) = std::fs::remove_file(path)
        && err.kind() != std::io::ErrorKind::NotFound
    {
        tracing::warn!(path = %path.display(), error = %err, "failed to delete local copy");
    }
}

fn remove_local_dir(path: &Path) {
    if let Err(err) = std::fs::remove_dir_all(path)
        && err.kind() != std::io::ErrorKind::NotFound
    {
        tracing::warn!(path = %path.display(), error = %err, "failed to delete local directory");
    }
}

/// A mailbox relpath as an object-key prefix: `/`-joined regardless of host
/// separator, with a trailing `/` so a UID appends directly. Empty for a
/// mailbox that sanitizes to nothing.
fn relative_key(mailbox_relpath: &Path) -> String {
    let joined = mailbox_relpath
        .components()
        .map(|component| component.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/");
    if joined.is_empty() {
        joined
    } else {
        format!("{joined}/")
    }
}

/// Runs the whole job: one interleaved queue across every identity, a
/// derived number of workers, and one shared pacer, push semaphore, and
/// pair of gates.
///
/// Deliberately never calls `set_macro_phase` (CLI-0146 §8, following
/// CLI-0130's precedent for `transform`): that gauge encodes CLI-0019's
/// one-way local-work-then-upload transition, which a job pushing inline
/// never performs.
pub(crate) async fn run_email_import_job(
    job: ImportJob,
    pending_by_identity: Vec<Vec<PendingMailbox>>,
    push_concurrency: usize,
) -> Result<JobSummary, String> {
    let ImportJob {
        contexts,
        destination,
        messages_per_minute,
        batch_size,
        max_connections,
        log_dir,
    } = job;

    // Interleaved round-robin rather than identity-by-identity: a
    // front-loaded queue would stall every worker on one identity's
    // connection semaphore (CLI-0071).
    let mut per_identity: Vec<VecDeque<Batch>> = pending_by_identity
        .iter()
        .map(|pending| super::batches_from_pending(pending, batch_size).into())
        .collect();
    let mut all_batches: VecDeque<QueueItem> = VecDeque::new();
    loop {
        let mut any_left = false;
        for (index, batches) in per_identity.iter_mut().enumerate() {
            if let Some(batch) = batches.pop_front() {
                all_batches.push_back((index, batch, BATCH_RETRIES));
                any_left = true;
            }
        }
        if !any_left {
            break;
        }
    }

    let total_batches = all_batches.len();
    let total_messages: usize = all_batches
        .iter()
        .map(|(_, batch, _)| batch.uids.len())
        .sum();

    // No `--concurrency` flag (CLI-0146 §1): throughput is the pacer's job,
    // so worker count only needs to be enough to keep every identity's
    // allowed connections busy.
    let worker_count = (max_connections * contexts.len().max(1))
        .max(1)
        .min(total_batches.max(1));

    tracing::info!(
        pigeon_job = JOB,
        identities = contexts.len(),
        batches = total_batches,
        messages = total_messages,
        workers = worker_count,
        messages_per_minute,
        batch_size,
        push_concurrency,
        "email-import: starting delivery"
    );

    let contexts = Arc::new(contexts);
    let identity_semaphores: Arc<Vec<Arc<Semaphore>>> = Arc::new(
        contexts
            .iter()
            .map(|ctx| {
                // CLI-0080's per-identity override still wins over this
                // job's own default, exactly as for the sibling jobs.
                let cap = ctx
                    .identity
                    .max_imap_connections
                    .map(|value| value as usize)
                    .unwrap_or(max_connections);
                Arc::new(Semaphore::new(cap.max(1)))
            })
            .collect(),
    );

    let push_log_dir = log_dir.join("push-logs");
    let shared = Arc::new(Shared {
        contexts,
        identity_semaphores,
        pacer: Arc::new(super::pace::Pacer::new(messages_per_minute)),
        imap_gate: Arc::new(RecoveryGate::new(
            "imap",
            IMAP_RECOVERY_ROUNDS,
            IMAP_RECOVERY_BACKOFF,
        )),
        push_gate: Arc::new(RecoveryGate::new(
            "push",
            PUSH_RECOVERY_ROUNDS,
            PUSH_RECOVERY_BACKOFF,
        )),
        push_semaphore: Arc::new(Semaphore::new(push_concurrency.max(1))),
        destination: Arc::new(destination.address()),
        push_log_dir: Arc::new(push_log_dir),
        push_concurrency,
    });

    let queue = Arc::new(Mutex::new(all_batches));
    let multi_progress = MultiProgress::new();
    let started = Instant::now();

    // CLI-0141: `tokio::spawn` drops the ambient span, so every per-message
    // warning would otherwise land in `pigeon.jsonl` with no `command`
    // field and be invisible to the `analyze-job-run` skill's documented
    // filter. Captured and re-attached here rather than discovered from a
    // failed run, which is what CLI-0098 and CLI-0141 both had to do.
    let command_span = tracing::Span::current();

    let mut handles = Vec::with_capacity(worker_count);
    for _ in 0..worker_count {
        let queue = Arc::clone(&queue);
        let shared = Arc::clone(&shared);
        let multi_progress = multi_progress.clone();
        handles.push(tokio::spawn(
            run_worker(queue, shared, multi_progress).instrument(command_span.clone()),
        ));
    }

    let mut summary = JobSummary::default();
    let mut first_error = None;
    for handle in handles {
        match handle.await {
            Ok(outcome) => {
                summary.delivered += outcome.delivered;
                summary.failed += outcome.failed;
                summary.objects_pushed += outcome.objects_pushed;
                summary.attachments_pushed += outcome.attachments_pushed;
                summary.attachment_extraction_failed += outcome.attachment_extraction_failed;
                summary.gate_give_ups += outcome.gate_give_ups;
                summary.failure_breakdown.merge(&outcome.failure_breakdown);
            }
            Err(err) => {
                tracing::error!(error = %err, "worker task panicked");
                first_error = Some(format!("worker task panicked: {err}"));
            }
        }
    }

    if let Some(err) = first_error {
        return Err(err);
    }

    tracing::info!(
        pigeon_job = JOB,
        delivered = summary.delivered,
        failed = summary.failed,
        objects_pushed = summary.objects_pushed,
        attachments_pushed = summary.attachments_pushed,
        attachment_extraction_failed = summary.attachment_extraction_failed,
        gate_give_ups = summary.gate_give_ups,
        elapsed_ms = started.elapsed().as_millis(),
        "email-import: run complete"
    );

    Ok(summary)
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
        assert!(
            requeue_or_none(0, test_batch(), 0, Duration::from_millis(1))
                .await
                .is_none()
        );
    }

    #[tokio::test]
    async fn requeue_or_none_requeues_with_one_fewer_attempt_remaining() {
        let (identity_index, batch, attempts_remaining) =
            requeue_or_none(2, test_batch(), BATCH_RETRIES, Duration::from_millis(1))
                .await
                .unwrap();

        assert_eq!(identity_index, 2);
        assert_eq!(batch.mailbox, "INBOX");
        assert_eq!(attempts_remaining, BATCH_RETRIES - 1);
    }

    #[test]
    fn relative_key_joins_with_slashes_and_adds_a_trailing_separator() {
        assert_eq!(
            relative_key(&PathBuf::from("archive").join("2020")),
            "archive/2020/"
        );
        assert_eq!(relative_key(&PathBuf::from("inbox")), "inbox/");
    }

    #[test]
    fn relative_key_is_empty_for_an_empty_relpath() {
        // An empty prefix must not produce a leading `//` in the object key.
        assert_eq!(relative_key(&PathBuf::new()), "");
    }

    /// The connect budget must be the patient one CLI-0144's evidence calls
    /// for, not the sibling jobs' 15 seconds. `retry_with_backoff` sleeps
    /// `backoff * attempt` before every attempt after the first, so the
    /// final attempt starts three minutes in.
    #[test]
    fn the_connect_budget_is_three_minutes_of_linear_backoff() {
        let total: Duration = (1..CONNECT_RETRIES)
            .map(|attempt| CONNECT_BACKOFF * attempt as u32)
            .sum();

        assert_eq!(total, Duration::from_secs(180));
        assert!(
            total >= Duration::from_secs(74),
            "must outlast the 74-second outage CLI-0144 measured"
        );
    }

    /// The push gate's own budget: 30+60+90+120.
    #[test]
    fn the_push_recovery_budget_is_five_minutes() {
        let total: Duration = (1..=PUSH_RECOVERY_ROUNDS)
            .map(|round| PUSH_RECOVERY_BACKOFF * round as u32)
            .sum();
        assert_eq!(total, Duration::from_secs(300));
    }

    /// The IMAP gate's budget: 60+120+180+240+300+360.
    #[test]
    fn the_imap_recovery_budget_is_about_twenty_one_minutes() {
        let total: Duration = (1..=IMAP_RECOVERY_ROUNDS)
            .map(|round| IMAP_RECOVERY_BACKOFF * round as u32)
            .sum();
        assert_eq!(total, Duration::from_secs(21 * 60));
    }

    #[test]
    fn remove_local_copy_is_a_no_op_for_a_missing_file() {
        let dir = tempfile::tempdir().unwrap();
        // Must not panic, and must not log at error level for the common
        // already-gone case.
        remove_local_copy(&dir.path().join("absent.eml"));
        remove_local_dir(&dir.path().join("absent"));
    }

    #[test]
    fn remove_local_copy_deletes_a_file_and_a_uid_directory() {
        let dir = tempfile::tempdir().unwrap();
        let eml = dir.path().join("1.eml");
        let uid_dir = dir.path().join("1");
        std::fs::create_dir_all(uid_dir.join("attachments")).unwrap();
        std::fs::write(&eml, b"x").unwrap();
        std::fs::write(uid_dir.join("attachments").join("a.pdf"), b"y").unwrap();

        remove_local_copy(&eml);
        remove_local_dir(&uid_dir);

        assert!(!eml.exists());
        assert!(!uid_dir.exists());
    }
}
