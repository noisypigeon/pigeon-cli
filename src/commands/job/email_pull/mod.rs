//! `pigeon job run email-pull` (ADR-0081): fetches raw `.eml` files and
//! unpacked attachments for one or more authenticated email identities --
//! no Markdown/frontmatter transform, no encryption, ever (§4 of the ADR).
//! Deduplicates attachments only, by content hash; raw `.eml` files are
//! never deduped against each other and are written straight to their
//! final location (no staging/rename step).
//!
//! Two directory trees exist per identity, mirrored by the same
//! `mailbox_relpath`: `staging_dir` holds bookkeeping only (`.uidvalidity`,
//! `.job-checkpoint`, `.attachment-hashes`, and attachment scratch files
//! awaiting the dedup/placement pass) and is never uploaded; `output_dir`
//! (nested one level deeper under an email-sanitized `identity_dir`, same
//! convention `email_sync` uses) holds the real deliverable -- placed
//! `.eml` files and the deduped `attachments/` folder -- and is what
//! `pending_upload_tasks` walks.

pub mod dedup;
pub mod manifest;
pub mod wizard;
mod worker;

use std::collections::HashSet;
use std::fs;

use async_imap::types::NameAttribute;
use futures::TryStreamExt;
use indicatif::MultiProgress;

use crate::commands::job::email_sync::sink;
use crate::commands::job::email_sync::{
    IdentityContext, IdentityManifestSummary, PendingMailbox,
    manifest::{pull_manifest, save_manifest},
};
use crate::commands::keyring::bucket::store::BucketConfig;
use crate::commands::keyring::email::identity;
use crate::core::job::Job;

use worker::JobSummary;

/// Connects to `ctx`'s identity, lists its mailboxes, and for each one:
/// resets on a `UIDVALIDITY` change, computes pending UIDs (server minus
/// already-checkpointed), and pulls a fresh size/attachment-count manifest
/// via `email_sync::manifest::pull_manifest` (reused directly -- pure IMAP
/// mechanics, no Markdown coupling). Near-identical to
/// `email_sync::gather_pending`, differing only in: which checkpoint
/// module it reads (this job's own 3-field `manifest::load_checkpoint`/
/// `done_uids`, not email-sync's 7-field one); and clearing *two*
/// directories on staleness -- `ctx.staging_dir`'s mirror (holding only
/// the `.uidvalidity` marker) and the *final* `identity_dir` mailbox
/// directory (holding the real `.eml` files) -- per this module's
/// top-level doc comment on the staging/output split.
pub(crate) async fn gather_pending(
    ctx: &IdentityContext,
    multi_progress: &MultiProgress,
) -> Result<(Vec<PendingMailbox>, IdentityManifestSummary), String> {
    let _ = multi_progress.println(format!("Connecting to {}...", ctx.identity.alias));
    let mut session = worker::connect_with_retry(ctx).await?;

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

    let identity_dir = ctx
        .output_dir
        .join(identity::sanitize_segment(&ctx.identity.email));

    let mut pending_mailboxes = Vec::new();
    let mut fresh_manifest = Vec::new();
    let mut summary = IdentityManifestSummary {
        alias: ctx.identity.alias.clone(),
        ..Default::default()
    };

    let bar = sink::new_progress_bar(
        format!("{} manifest", ctx.identity.alias),
        mailboxes.len() as u64,
        multi_progress,
    );

    for (mailbox_name, delimiter) in &mailboxes {
        let mailbox_relpath = sink::sanitize_mailbox_path(mailbox_name, delimiter.as_deref());
        let staging_mailbox_dir = ctx.staging_dir.join(&mailbox_relpath);
        fs::create_dir_all(&staging_mailbox_dir)
            .map_err(|err| format!("failed to create {}: {err}", staging_mailbox_dir.display()))?;

        let mailbox_response = session
            .examine(mailbox_name)
            .await
            .map_err(|err| format!("failed to open '{mailbox_name}' read-only: {err}"))?;
        let current_validity = mailbox_response.uid_validity.unwrap_or(0);
        if sink::is_stale(
            sink::read_uidvalidity(&staging_mailbox_dir),
            current_validity,
        ) {
            sink::clear_eml_files(&staging_mailbox_dir)?;
            sink::clear_eml_files(&identity_dir.join(&mailbox_relpath))?;
            manifest::clear_checkpoint_for_mailbox(&ctx.staging_dir, mailbox_name)?;
        }
        sink::write_uidvalidity(&staging_mailbox_dir, current_validity)?;

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
            bar.inc(1);
            continue;
        }

        let mailbox_manifest = pull_manifest(&mut session, mailbox_name, &pending_uids).await?;

        summary.mailboxes += 1;
        summary.pending_messages += mailbox_manifest.len();
        summary.pending_bytes += mailbox_manifest.iter().map(|entry| entry.size).sum::<u64>();
        summary.pending_attachments += mailbox_manifest
            .iter()
            .map(|entry| entry.attachments as usize)
            .sum::<usize>();
        fresh_manifest.extend(mailbox_manifest);

        pending_mailboxes.push(PendingMailbox {
            mailbox: mailbox_name.clone(),
            mailbox_relpath,
            uids: pending_uids,
        });
        bar.inc(1);
    }
    bar.finish();

    // Best-effort (ADR-0068): every mailbox's manifest is already gathered
    // by this point, so a logout-time disconnect must not discard it.
    let _ = session.logout().await;

    save_manifest(&ctx.staging_dir, &fresh_manifest)?;

    Ok((pending_mailboxes, summary))
}

/// The `Job` implementor for `pigeon job run email-pull`. No `encryptor`
/// field at all -- ADR-0081 §4's explicit, permanent scoping decision, not
/// a temporarily-unused slot.
pub(crate) struct PullJob {
    pub contexts: Vec<IdentityContext>,
    pub remote: Option<(BucketConfig, String)>,
    pub max_connections_per_identity: usize,
}

pub(crate) struct PullPlan {
    pub pending_by_identity: Vec<Vec<PendingMailbox>>,
    pub manifest_summaries: Vec<IdentityManifestSummary>,
}

impl Job for PullJob {
    type Plan = PullPlan;
    type Summary = JobSummary;

    async fn gather(&self) -> Result<PullPlan, String> {
        let multi_progress = MultiProgress::new();
        let mut pending_by_identity = Vec::with_capacity(self.contexts.len());
        let mut manifest_summaries = Vec::with_capacity(self.contexts.len());
        for ctx in &self.contexts {
            let (pending, summary) = gather_pending(ctx, &multi_progress).await?;
            pending_by_identity.push(pending);
            manifest_summaries.push(summary);
        }
        Ok(PullPlan {
            pending_by_identity,
            manifest_summaries,
        })
    }

    async fn run(
        self,
        plan: PullPlan,
        concurrency: usize,
        upload_concurrency: usize,
    ) -> Result<JobSummary, String> {
        let PullJob {
            contexts,
            remote,
            max_connections_per_identity,
        } = self;
        let remote_ref = remote
            .as_ref()
            .map(|(bucket_config, secret)| (bucket_config, secret.as_str()));
        worker::run_email_pull_job(
            contexts,
            plan.pending_by_identity,
            concurrency,
            upload_concurrency,
            max_connections_per_identity,
            remote_ref,
        )
        .await
    }
}
