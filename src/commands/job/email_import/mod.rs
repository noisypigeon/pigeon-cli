//! `pigeon job run email import` (CLI-0146): reads one or more
//! authenticated IMAP identities and delivers each message's raw `.eml`
//! plus its unpacked attachments to an rclone-addressed `--destination`,
//! at a declared, rate-limit-respecting pace.
//!
//! Deliberately minimal: no deduplication, no indexing, no renaming, no
//! content transform, and no encryption. It moves bytes and nothing else
//! (CLI-0146 Out of scope).
//!
//! The shape that distinguishes it from `email-sync`/`email-pull` is
//! per-message delivery: fetch -> push -> **delete the local copy**, on
//! success and on failure alike. There is no staged-then-published tree and
//! no separate upload phase, so local disk is bounded to the in-flight
//! window unconditionally, a long run delivers incrementally rather than
//! all at the end, and `--upload-only` has nothing to resume (CLI-0146 §5).
//!
//! What counts as delivered is the durable ingest ledger in
//! `--ledger-bucket` by default (CLI-0164 §7, `ledger.rs`), seeded from the
//! destination the first time and pushed periodically during a run; under
//! `--legacy` it is CLI-0146's local `.pushed` checkpoint alone.
//!
//! One directory exists per identity under `--local-output`:
//! `<alias>/.staging/` holds bookkeeping only (the `.pushed` checkpoint,
//! the ledger's working copy, and per-mailbox `.uidvalidity` markers) and
//! `<alias>/scratch/` holds the in-flight messages, which the GC keeps
//! near-empty.

mod attach;
pub mod gate;
pub mod ledger;
pub mod manifest;
pub mod pace;
mod push;
pub mod sink;
pub mod wizard;
pub(crate) mod worker;

use std::collections::HashSet;
use std::fs;
use std::path::PathBuf;
use std::sync::OnceLock;

use async_imap::types::NameAttribute;
use futures::TryStreamExt;
use indicatif::MultiProgress;

use crate::commands::job::progress::new_progress_bar;
use crate::commands::keyring::email::identity::Identity;
use crate::core::job::Job;
use crate::core::rclone::config::RemoteTarget;

use ledger::{LedgerSync, SeedMailbox};
use manifest::Batch;
use worker::JobSummary;

/// How many simultaneous IMAP connections this job opens to any one
/// identity by default.
///
/// Deliberately far below `email_sync`'s `DEFAULT_MAX_CONNECTIONS_PER_IDENTITY`
/// of 6 (CLI-0071, chosen to sit under Gmail's documented 15-connection
/// cap): this job's premise is a run that must stay welcome on a server for
/// days, and its throughput comes from the pacer rather than from
/// parallelism, so additional connections buy nothing and only widen the
/// blast radius of a provider's connection limit.
pub(crate) const DEFAULT_MAX_CONNECTIONS: usize = 2;

/// One identity's resolved run state.
///
/// Its own type rather than `email_sync::IdentityContext`, which carries a
/// `staging_dir`/`output_dir` split this job has no equivalent of.
pub(crate) struct ImportContext {
    pub identity: Identity,
    pub secret: String,
    /// Bookkeeping only: the `.pushed` checkpoint and `.uidvalidity`
    /// markers. Never uploaded.
    pub staging_dir: PathBuf,
    /// Where in-flight messages land between fetch and push. Kept
    /// near-empty by the GC (CLI-0146 §5).
    pub scratch_dir: PathBuf,
    /// This identity's durable ledger handle, set by `gather_pending` once
    /// the ledger is loaded or seeded. Never set under `--legacy`
    /// (CLI-0164 §7).
    pub ledger: OnceLock<LedgerSync>,
}

/// Where `email import` keeps its record of what is delivered (CLI-0164
/// §7).
pub(crate) enum ImportMode {
    /// CLI-0146's original behavior: the local `.pushed` checkpoint only.
    Legacy,
    /// The durable ledger in `--ledger-bucket` decides what is pending and
    /// records each delivery; `.pushed` is still written alongside it.
    Ledger(RemoteTarget),
}

/// One identity's pre-run counts, for the wizard table.
///
/// No byte or attachment estimate: producing one costs a `BODYSTRUCTURE`
/// fetch per pending UID, a second full-corpus traversal that at this job's
/// throttled pace costs about as much as the fetch it would be estimating
/// (CLI-0146 §7, inverting CLI-0032/0034's reasoning for this job only).
#[derive(Debug, Default)]
pub(crate) struct IdentitySummary {
    pub alias: String,
    pub mailboxes: usize,
    pub pending_messages: usize,
}

/// One mailbox's pending (not yet delivered) UIDs.
pub(crate) struct PendingMailbox {
    pub mailbox: String,
    pub mailbox_relpath: PathBuf,
    pub uidvalidity: u32,
    pub uids: Vec<u32>,
}

/// One selectable mailbox as the server reports it right now: the cheap
/// part both `email import` and `email scan` need (CLI-0164 §5) -- one
/// `EXAMINE` and one `UID SEARCH`, no per-message fetch.
pub(crate) struct MailboxState {
    pub mailbox: String,
    pub mailbox_relpath: PathBuf,
    pub uidvalidity: u32,
    pub server_uids: HashSet<u32>,
}

impl MailboxState {
    pub(crate) fn seed_mailbox(&self) -> SeedMailbox {
        SeedMailbox {
            mailbox: self.mailbox.clone(),
            relpath_key: ledger::relpath_key(&self.mailbox_relpath),
            uidvalidity: self.uidvalidity,
        }
    }
}

/// Lists every selectable mailbox on `session` and reads each one's
/// `UIDVALIDITY` and full UID set, read-only (`EXAMINE`, CLI-0005).
pub(crate) async fn list_mailbox_states(
    session: &mut crate::commands::keyring::email::imap_client::ImapSession,
    label: &str,
    multi_progress: &MultiProgress,
) -> Result<Vec<MailboxState>, String> {
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

    let bar = new_progress_bar(
        format!("{label} scan"),
        mailboxes.len() as u64,
        multi_progress,
    );
    let mut states = Vec::with_capacity(mailboxes.len());
    for (mailbox_name, delimiter) in &mailboxes {
        let mailbox_response = session
            .examine(mailbox_name)
            .await
            .map_err(|err| format!("failed to open '{mailbox_name}' read-only: {err}"))?;
        let server_uids: HashSet<u32> = session
            .uid_search("ALL")
            .await
            .map_err(|err| format!("failed to search '{mailbox_name}': {err}"))?;
        states.push(MailboxState {
            mailbox: mailbox_name.clone(),
            mailbox_relpath: sink::sanitize_mailbox_path(mailbox_name, delimiter.as_deref()),
            uidvalidity: mailbox_response.uid_validity.unwrap_or(0),
            server_uids,
        });
        bar.inc(1);
    }
    bar.finish();
    Ok(states)
}

/// Connects to `ctx`'s identity, lists its mailboxes, and computes each
/// one's pending UIDs as "every UID the server reports, minus every one
/// already delivered".
///
/// "Already delivered" is the durable ledger under `ImportMode::Ledger`
/// (loaded, or seeded from `destination` the first time -- CLI-0164 §4,
/// §7), and the local `.pushed` checkpoint under `--legacy`, reset on a
/// `UIDVALIDITY` change exactly as before.
///
/// Deliberately does **not** fetch `BODYSTRUCTURE` (CLI-0146 §7). The one
/// `UID SEARCH` per mailbox is needed for resume regardless and costs a
/// single round trip.
#[tracing::instrument(skip_all, fields(identity = %ctx.identity.alias, email = %ctx.identity.email))]
pub(crate) async fn gather_pending(
    ctx: &ImportContext,
    mode: &ImportMode,
    destination: &RemoteTarget,
    log_dir: &std::path::Path,
    multi_progress: &MultiProgress,
) -> Result<(Vec<PendingMailbox>, IdentitySummary), String> {
    let _ = multi_progress.println(format!("Connecting to {}...", ctx.identity.alias));
    let mut session = worker::connect_with_retry(ctx).await?;
    let states = list_mailbox_states(&mut session, &ctx.identity.alias, multi_progress).await;
    // Best-effort (CLI-0068): every mailbox is already scanned by this
    // point, so a logout-time disconnect must not discard the result.
    let _ = session.logout().await;
    let states = states?;

    let mut summary = IdentitySummary {
        alias: ctx.identity.alias.clone(),
        ..Default::default()
    };
    let mut pending_mailboxes = Vec::new();

    let delivered = match mode {
        ImportMode::Ledger(ledger_target) => {
            let seed: Vec<SeedMailbox> = states.iter().map(MailboxState::seed_mailbox).collect();
            let local_path = ctx.staging_dir.join(ledger::LEDGER_FILE);
            let (loaded, _seeded) = ledger::load_or_seed(
                ledger_target,
                Some(destination),
                &ctx.identity.alias,
                &ctx.identity.email,
                &seed,
                &local_path,
                log_dir,
                worker::JOB,
            )
            .await?;
            // The working copy every delivery appends to and every flush
            // pushes whole, so it starts as the full history.
            ledger::write_local(&loaded, &local_path)?;
            let _ = ctx.ledger.set(LedgerSync::new(
                ledger::identity_dir_address(ledger_target, &ctx.identity.email),
                local_path,
                log_dir.join("ledger-logs"),
            ));
            Delivered::Ledger(loaded)
        }
        ImportMode::Legacy => {
            for state in &states {
                let marker_dir = ctx.staging_dir.join(&state.mailbox_relpath);
                fs::create_dir_all(&marker_dir)
                    .map_err(|err| format!("failed to create {}: {err}", marker_dir.display()))?;
                if sink::is_stale(sink::read_uidvalidity(&marker_dir), state.uidvalidity) {
                    // Every UID this mailbox ever reported is now
                    // meaningless. Only the checkpoint needs clearing --
                    // there is no local `.eml` tree to invalidate, since
                    // nothing survives its own delivery (CLI-0146 §5).
                    manifest::clear_checkpoint_for_mailbox(&ctx.staging_dir, &state.mailbox)?;
                }
                sink::write_uidvalidity(&marker_dir, state.uidvalidity)?;
            }
            Delivered::Checkpoint(manifest::done_uids(&manifest::load_checkpoint(
                &ctx.staging_dir,
            )?))
        }
    };

    for state in &states {
        let pending_uids = sink::missing_uids(&state.server_uids, &delivered.uids_for(state));
        if pending_uids.is_empty() {
            continue;
        }
        summary.mailboxes += 1;
        summary.pending_messages += pending_uids.len();
        pending_mailboxes.push(PendingMailbox {
            mailbox: state.mailbox.clone(),
            mailbox_relpath: state.mailbox_relpath.clone(),
            uidvalidity: state.uidvalidity,
            uids: pending_uids,
        });
    }

    Ok((pending_mailboxes, summary))
}

/// What `gather_pending` treats as already delivered, per mode.
enum Delivered {
    Ledger(ledger::Ledger),
    /// `--legacy`'s `.pushed`, keyed `(mailbox, uid)` exactly as CLI-0146
    /// §6 resumed.
    Checkpoint(HashSet<(String, u32)>),
}

impl Delivered {
    fn uids_for(&self, state: &MailboxState) -> HashSet<u32> {
        match self {
            Delivered::Ledger(ledger) => ledger.ingested_uids(&state.mailbox, state.uidvalidity),
            Delivered::Checkpoint(done) => done
                .iter()
                .filter(|(mailbox, _)| mailbox == &state.mailbox)
                .map(|(_, uid)| *uid)
                .collect(),
        }
    }
}

/// Turns one identity's pending mailboxes into `--batch-size`d batches.
pub(crate) fn batches_from_pending(pending: &[PendingMailbox], batch_size: usize) -> Vec<Batch> {
    pending
        .iter()
        .flat_map(|mailbox| {
            manifest::split_into_batches(&mailbox.uids, batch_size)
                .into_iter()
                .map(|uids| Batch {
                    mailbox: mailbox.mailbox.clone(),
                    mailbox_relpath: mailbox.mailbox_relpath.clone(),
                    uidvalidity: mailbox.uidvalidity,
                    uids,
                })
        })
        .collect()
}

/// The `Job` implementor. No `encryptor` field and no `remote: Option<..>`:
/// the destination is mandatory and encryption is permanently out of scope
/// (CLI-0146 §1).
pub(crate) struct ImportJob {
    pub contexts: Vec<ImportContext>,
    pub mode: ImportMode,
    pub destination: RemoteTarget,
    pub messages_per_minute: usize,
    pub batch_size: usize,
    pub max_connections: usize,
    pub log_dir: PathBuf,
}

pub(crate) struct ImportPlan {
    pub pending_by_identity: Vec<Vec<PendingMailbox>>,
    pub summaries: Vec<IdentitySummary>,
}

impl Job for ImportJob {
    type Plan = ImportPlan;
    type Summary = JobSummary;

    async fn gather(&self) -> Result<ImportPlan, String> {
        let multi_progress = MultiProgress::new();
        let mut pending_by_identity = Vec::with_capacity(self.contexts.len());
        let mut summaries = Vec::with_capacity(self.contexts.len());
        for ctx in &self.contexts {
            let (pending, summary) = gather_pending(
                ctx,
                &self.mode,
                &self.destination,
                &self.log_dir,
                &multi_progress,
            )
            .await?;
            pending_by_identity.push(pending);
            summaries.push(summary);
        }
        Ok(ImportPlan {
            pending_by_identity,
            summaries,
        })
    }

    /// `concurrency` is ignored -- this job has no `--concurrency` flag
    /// (CLI-0146 §1); worker count is derived from `--max-connections` and
    /// the identity count. `upload_concurrency` carries
    /// `--push-concurrency`, the one transfer knob that applies.
    async fn run(
        self,
        plan: ImportPlan,
        _concurrency: usize,
        push_concurrency: usize,
    ) -> Result<JobSummary, String> {
        worker::run_email_import_job(self, plan.pending_by_identity, push_concurrency).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pending(mailbox: &str, uids: Vec<u32>) -> PendingMailbox {
        PendingMailbox {
            mailbox: mailbox.to_string(),
            mailbox_relpath: PathBuf::from(mailbox.to_ascii_lowercase()),
            uidvalidity: 7,
            uids,
        }
    }

    #[test]
    fn batches_from_pending_chunks_each_mailbox_at_the_batch_size() {
        let pending = vec![
            pending("INBOX", (1..=25).collect()),
            pending("Archive", vec![1, 2]),
        ];

        let batches = batches_from_pending(&pending, 10);

        // 25 -> 10/10/5, plus Archive's single batch of 2.
        assert_eq!(batches.len(), 4);
        assert_eq!(batches[0].uids.len(), 10);
        assert_eq!(batches[2].uids.len(), 5);
        assert_eq!(batches[3].mailbox, "Archive");
        assert_eq!(batches[3].uids, vec![1, 2]);
    }

    #[test]
    fn batches_from_pending_carries_each_mailboxs_relpath_through() {
        let batches = batches_from_pending(&[pending("INBOX", vec![1])], 10);

        assert_eq!(batches[0].mailbox_relpath, PathBuf::from("inbox"));
        assert_eq!(batches[0].uidvalidity, 7);
    }

    #[test]
    fn batches_from_pending_is_empty_for_no_pending_work() {
        assert!(batches_from_pending(&[], 10).is_empty());
        assert!(batches_from_pending(&[pending("INBOX", Vec::new())], 10).is_empty());
    }

    /// The default connection cap is deliberately conservative, since
    /// throughput here comes from the pacer rather than from parallelism.
    ///
    /// This used to assert `DEFAULT_MAX_CONNECTIONS < email_sync`'s own
    /// default of 6 (CLI-0071), which is how the "deliberately lower"
    /// intent was pinned. CLI-0147 removed that job, so the comparison has
    /// no referent and the value is pinned directly instead -- 6 recorded
    /// here as the historical figure it was chosen against.
    #[test]
    fn default_max_connections_is_conservative() {
        assert_eq!(DEFAULT_MAX_CONNECTIONS, 2);
    }
}
