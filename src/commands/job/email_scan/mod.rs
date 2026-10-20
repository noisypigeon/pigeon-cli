//! `pigeon job run email scan` (CLI-0164): compares each identity's IMAP
//! account against its durable ingest ledger and records what is new,
//! without transferring a single message body.
//!
//! Built to run from a scheduler: per identity it costs one connection,
//! one `EXAMINE` and one `UID SEARCH` per mailbox, a ledger download, and
//! -- only when something is new -- a `BODYSTRUCTURE` fetch over just the
//! new UIDs to estimate their attachments. The result is written to
//! `<ledger>/<sanitized-email>/scan-status.tsv`, beside the ledger.
//!
//! Scan reads the ledger and never appends to it: only a confirmed
//! delivery by `email import` adds a row (CLI-0164 §3). The one exception
//! is the first sight of an identity with no ledger, which is seeded from
//! `--destination`'s existing `.eml` objects (CLI-0164 §4).

pub(crate) mod bodystructure;
pub(crate) mod status;
pub mod wizard;

use std::path::Path;

use indicatif::MultiProgress;

use crate::commands::job::email_import::ledger::{self, SeedMailbox};
use crate::commands::job::email_import::{
    ImportContext, MailboxState, list_mailbox_states, worker,
};
use crate::core::rclone::config::RemoteTarget;

use status::MailboxStatus;

pub(crate) const JOB: &str = "email-scan";

/// One identity's scan result.
#[derive(Debug)]
pub(crate) struct IdentityScan {
    pub alias: String,
    /// One row per mailbox, then the `*` total row.
    pub rows: Vec<MailboxStatus>,
    /// Whether this scan created the ledger.
    pub seeded: bool,
    /// UIDs whose `BODYSTRUCTURE` could not be parsed (CLI-0065).
    pub unparsed_bodystructures: usize,
}

impl IdentityScan {
    pub(crate) fn total(&self) -> Option<&MailboxStatus> {
        self.rows.last()
    }
}

/// Scans one identity and publishes its `scan-status.tsv`.
#[tracing::instrument(skip_all, fields(identity = %ctx.identity.alias, email = %ctx.identity.email))]
pub(crate) async fn scan_identity(
    ctx: &ImportContext,
    ledger_target: &RemoteTarget,
    destination: Option<&RemoteTarget>,
    log_dir: &Path,
    multi_progress: &MultiProgress,
) -> Result<IdentityScan, String> {
    let alias = &ctx.identity.alias;
    tracing::info!(identity = %alias, "email-scan: scanning identity");
    let _ = multi_progress.println(format!("Connecting to {alias}..."));

    let mut session = connect(ctx).await?;
    let states = list_mailbox_states(&mut session, alias, multi_progress).await;
    // Logged out before any rclone work: a ledger seed can take minutes,
    // longer than a server should be asked to hold an idle session. A
    // second connection is opened only if there is something to count.
    let _ = session.logout().await;
    let states = states?;
    crate::observability::metrics::record_phase_count(
        JOB,
        "examine",
        "ok",
        states.len() as u64,
        None,
    );

    let seed: Vec<SeedMailbox> = states.iter().map(MailboxState::seed_mailbox).collect();
    let (ledger, seeded) = ledger::load_or_seed(
        ledger_target,
        destination,
        alias,
        &ctx.identity.email,
        &seed,
        &ctx.staging_dir.join(ledger::LEDGER_FILE),
        log_dir,
        JOB,
    )
    .await?;

    let scanned_at = ledger::now_timestamp();
    let mut rows = Vec::with_capacity(states.len() + 1);
    let mut new_by_mailbox: Vec<(usize, Vec<u32>)> = Vec::new();
    for state in &states {
        let ingested = ledger.ingested_uids(&state.mailbox, state.uidvalidity);
        let (row, new_uids) = status::diff(
            &state.mailbox,
            state.uidvalidity,
            &state.server_uids,
            &ingested,
            &scanned_at,
        );
        if !new_uids.is_empty() {
            new_by_mailbox.push((rows.len(), new_uids));
        }
        rows.push(row);
    }

    let total_new: usize = new_by_mailbox.iter().map(|(_, uids)| uids.len()).sum();
    let mut unparsed_bodystructures = 0;
    if total_new > 0 && !bodystructure::within_cap(total_new) {
        tracing::info!(
            identity = %alias,
            new_messages = total_new,
            cap = bodystructure::ATTACHMENT_COUNT_CAP,
            "email-scan: too many new messages to count attachments; reporting them as unknown"
        );
        for (index, _) in &new_by_mailbox {
            rows[*index].new_attachments = None;
        }
    } else if total_new > 0 {
        let mut session = connect(ctx).await?;
        for (index, uids) in &new_by_mailbox {
            let mailbox = rows[*index].mailbox.clone();
            match session.examine(&mailbox).await {
                Ok(_) => {
                    let count =
                        bodystructure::count_new_attachments(&mut session, &mailbox, uids).await;
                    rows[*index].new_attachments = Some(count.attachments);
                    unparsed_bodystructures += count.unparsed;
                    crate::observability::metrics::record_phase_count(
                        JOB,
                        "bodystructure",
                        "ok",
                        uids.len() as u64,
                        None,
                    );
                }
                Err(err) => {
                    tracing::warn!(identity = %alias, mailbox = %mailbox, error = %err, "email-scan: could not reopen mailbox to count attachments");
                    rows[*index].new_attachments = None;
                    crate::observability::metrics::record_phase(
                        JOB,
                        "bodystructure",
                        "failed",
                        None,
                    );
                }
            }
        }
        let _ = session.logout().await;
    }

    rows.push(status::total(&rows, &scanned_at));

    let dir_address = ledger::identity_dir_address(ledger_target, &ctx.identity.email);
    let status_path = ctx.staging_dir.join(status::STATUS_FILE);
    let previous = ledger::download_state(
        &dir_address,
        status::STATUS_FILE,
        &status_path,
        log_dir,
        JOB,
    )
    .await?
    .map(|contents| status::parse(&contents))
    .unwrap_or_default();
    status::carry_forward(&previous, &mut rows);

    std::fs::write(&status_path, status::serialize(&rows))
        .map_err(|err| format!("failed to write {}: {err}", status_path.display()))?;
    ledger::upload_state(
        &status_path,
        &dir_address,
        status::STATUS_FILE,
        log_dir,
        JOB,
    )
    .await?;
    crate::observability::metrics::record_phase(JOB, "state", "ok", None);

    let scan = IdentityScan {
        alias: alias.clone(),
        rows,
        seeded,
        unparsed_bodystructures,
    };
    if let Some(total) = scan.total() {
        tracing::info!(
            identity = %alias,
            mailboxes = states.len(),
            server_messages = total.server_messages,
            ingested = total.ingested,
            new_messages = total.new_messages,
            new_attachments = ?total.new_attachments,
            removed_on_server = total.removed_on_server,
            seeded,
            "email-scan: identity scanned"
        );
    }
    Ok(scan)
}

async fn connect(
    ctx: &ImportContext,
) -> Result<crate::commands::keyring::email::imap_client::ImapSession, String> {
    match worker::connect_with_retry(ctx).await {
        Ok(session) => {
            crate::observability::metrics::record_phase(JOB, "connect", "ok", None);
            Ok(session)
        }
        Err(err) => {
            crate::observability::metrics::record_phase(JOB, "connect", "failed", None);
            Err(err)
        }
    }
}
