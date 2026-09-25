pub mod dedup;
pub mod manifest;
pub mod sink;
pub mod transform;
pub mod wizard;
mod worker;

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::PathBuf;

use async_imap::types::NameAttribute;
use futures::TryStreamExt;

use crate::commands::keyring::bucket::store::BucketConfig;
use crate::commands::keyring::email::identity::Identity;
use crate::core::crypto::Aes256GcmSivEncryptor;
use crate::core::job::Job;
use manifest::{Batch, ManifestEntry};
use worker::JobSummary;

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

/// The `Job` implementor for `pigeon job run email-sync` (ADR-0023).
/// `remote` is resolved by the wizard before the final construction, once
/// every other input (including the pre-run manifest summary from
/// `gather()`) is known -- a job-specific concern, not something `Job`
/// itself needs to know about. `encryptor` is resolved right after
/// `remote`, from the target bucket-config's default key, an explicit
/// `--encryption-key` override, or an interactive choice (ADR-0027) --
/// `None` when uploading unencrypted or not uploading at all.
pub(crate) struct EmailSyncJob {
    pub contexts: Vec<IdentityContext>,
    pub remote: Option<(BucketConfig, String)>,
    pub encryptor: Option<Aes256GcmSivEncryptor>,
}

pub(crate) struct EmailSyncPlan {
    pub pending_by_identity: Vec<Vec<PendingMailbox>>,
    pub manifest_summaries: Vec<IdentityManifestSummary>,
}

impl Job for EmailSyncJob {
    type Plan = EmailSyncPlan;
    type Summary = JobSummary;

    async fn gather(&self) -> Result<EmailSyncPlan, String> {
        let mut pending_by_identity = Vec::with_capacity(self.contexts.len());
        let mut manifest_summaries = Vec::with_capacity(self.contexts.len());
        for ctx in &self.contexts {
            let (pending, summary) = gather_pending(ctx).await?;
            pending_by_identity.push(pending);
            manifest_summaries.push(summary);
        }
        Ok(EmailSyncPlan {
            pending_by_identity,
            manifest_summaries,
        })
    }

    async fn run(self, plan: EmailSyncPlan, concurrency: usize) -> Result<JobSummary, String> {
        let EmailSyncJob {
            contexts,
            remote,
            encryptor,
        } = self;
        let remote_ref = remote
            .as_ref()
            .map(|(bucket_config, secret)| (bucket_config, secret.as_str()));
        worker::run_email_sync_job(
            contexts,
            plan.pending_by_identity,
            concurrency,
            remote_ref,
            encryptor.as_ref(),
        )
        .await
    }
}
