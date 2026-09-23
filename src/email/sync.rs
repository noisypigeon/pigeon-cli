use std::collections::HashSet;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use async_imap::types::NameAttribute;
use futures::TryStreamExt;
use futures::stream::{self, StreamExt};
use indicatif::MultiProgress;

use crate::email::dedup::ContentIndex;
use crate::email::identity::Identity;
use crate::email::imap_client;
use crate::email::transform::TransformedMessage;
use crate::email::{sink, transform};
use crate::remote::client;
use crate::remote::store::Remote;

const PROCESSED_FILE_NAME: &str = ".processed";

/// Summary of a completed `sync` run.
#[derive(Debug, Default)]
pub struct SyncSummary {
    pub mailboxes: usize,
    pub synced: usize,
    pub already_processed: usize,
    pub failed: usize,
    pub uploaded: usize,
    pub unchanged: usize,
    pub upload_failed: usize,
    pub merged_messages: usize,
    pub deduped_attachments: usize,
}

impl SyncSummary {
    /// Adds `other`'s counts into `self` -- used to combine every
    /// concurrent mailbox worker's (ADR-0014) independently-accumulated
    /// summary into one final total.
    fn merge(&mut self, other: &SyncSummary) {
        self.mailboxes += other.mailboxes;
        self.synced += other.synced;
        self.already_processed += other.already_processed;
        self.failed += other.failed;
        self.uploaded += other.uploaded;
        self.unchanged += other.unchanged;
        self.upload_failed += other.upload_failed;
        self.merged_messages += other.merged_messages;
        self.deduped_attachments += other.deduped_attachments;
    }
}

/// Owned, per-run configuration cloned into every concurrent mailbox worker
/// (ADR-0014). Bundled into one struct, rather than passed as several
/// separate parameters, so each worker needs only one `.clone()`.
#[derive(Clone)]
struct SyncContext {
    identity: Identity,
    secret: String,
    staging_dir: PathBuf,
    output_dir: PathBuf,
    output_remote: Option<(Remote, String)>,
}

/// For every mailbox, for every message not yet in `.processed`: fetch it
/// (unless already staged), transform it, and -- only once the output is
/// verified -- delete the staged `.eml` and record its UID as processed.
/// Per ADR-0007, this is `pigeon email sync`'s default (no `--debug`)
/// behavior. Mailboxes are processed concurrently, up to `concurrency` at a
/// time (ADR-0014).
pub fn run(
    identity: &Identity,
    secret: &str,
    staging_dir: &Path,
    output_dir: &Path,
    output_remote: Option<(&Remote, &str)>,
    concurrency: usize,
) -> Result<SyncSummary, String> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|err| format!("failed to start async runtime: {err}"))?;

    runtime.block_on(run_async(
        identity,
        secret,
        staging_dir,
        output_dir,
        output_remote,
        concurrency,
    ))
}

async fn run_async(
    identity: &Identity,
    secret: &str,
    staging_dir: &Path,
    output_dir: &Path,
    output_remote: Option<(&Remote, &str)>,
    concurrency: usize,
) -> Result<SyncSummary, String> {
    let mut session = imap_client::connect_and_login(
        &identity.host,
        identity.port,
        &identity.email,
        secret,
        identity.provider.accepts_invalid_certs(),
    )
    .await?;

    let names: Vec<_> = session
        .list(None, Some("*"))
        .await
        .map_err(|err| format!("failed to list mailboxes: {err}"))?
        .try_collect()
        .await
        .map_err(|err| format!("failed to list mailboxes: {err}"))?;

    // ADR-0014: mailbox names are extracted to owned data before this
    // connection closes -- `async_imap::types::Name` borrows from its own
    // response and can't cross a `tokio::spawn` boundary. Each mailbox gets
    // its own connection below, so this one's only job was `LIST`.
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

    session
        .logout()
        .await
        .map_err(|err| format!("logout failed: {err}"))?;

    // ADR-0012: dedup spans every mailbox for this identity, so both
    // indexes are loaded once up front, not per-mailbox. ADR-0014: shared
    // across concurrent workers, so both are lock-guarded.
    let message_index = Arc::new(Mutex::new(ContentIndex::load(
        staging_dir,
        ContentIndex::MESSAGE_HASHES,
    )?));
    let attachment_index = Arc::new(Mutex::new(ContentIndex::load(
        staging_dir,
        ContentIndex::ATTACHMENT_HASHES,
    )?));
    let multi_progress = MultiProgress::new();

    let ctx = SyncContext {
        identity: identity.clone(),
        secret: secret.to_string(),
        staging_dir: staging_dir.to_path_buf(),
        output_dir: output_dir.to_path_buf(),
        output_remote: output_remote.map(|(remote, secret)| (remote.clone(), secret.to_string())),
    };

    let results: Vec<Result<SyncSummary, String>> = stream::iter(mailboxes)
        .map(|(mailbox_name, delimiter)| {
            let ctx = ctx.clone();
            let message_index = Arc::clone(&message_index);
            let attachment_index = Arc::clone(&attachment_index);
            let multi_progress = multi_progress.clone();
            tokio::spawn(async move {
                sync_mailbox(
                    ctx,
                    mailbox_name,
                    delimiter,
                    message_index,
                    attachment_index,
                    multi_progress,
                )
                .await
            })
        })
        .buffer_unordered(concurrency.max(1))
        .map(|joined| {
            joined.unwrap_or_else(|err| Err(format!("mailbox worker task panicked: {err}")))
        })
        .collect()
        .await;

    // Merge every mailbox that succeeded; surface the first failure, if
    // any, only after every worker has finished -- no mid-flight
    // cancellation, matching the prior sequential loop's simplest
    // fail-fast contract.
    let mut summary = SyncSummary::default();
    let mut first_error = None;
    for result in results {
        match result {
            Ok(mailbox_summary) => summary.merge(&mailbox_summary),
            Err(err) => {
                eprintln!("Error: {err}");
                if first_error.is_none() {
                    first_error = Some(err);
                }
            }
        }
    }

    match first_error {
        Some(err) => Err(err),
        None => Ok(summary),
    }
}

/// Fetches, transforms, verifies, uploads, and cleans up every pending
/// message in one mailbox, on its own IMAP connection -- ADR-0014's unit of
/// concurrency. `message_index`/`attachment_index` are shared with every
/// other concurrently-running mailbox worker.
async fn sync_mailbox(
    ctx: SyncContext,
    mailbox_name: String,
    delimiter: Option<String>,
    message_index: Arc<Mutex<ContentIndex>>,
    attachment_index: Arc<Mutex<ContentIndex>>,
    multi_progress: MultiProgress,
) -> Result<SyncSummary, String> {
    let SyncContext {
        identity,
        secret,
        staging_dir,
        output_dir,
        output_remote,
    } = ctx;
    let output_remote = output_remote
        .as_ref()
        .map(|(remote, secret)| (remote, secret.as_str()));

    let mut session = imap_client::connect_and_login(
        &identity.host,
        identity.port,
        &identity.email,
        &secret,
        identity.provider.accepts_invalid_certs(),
    )
    .await?;

    let mut summary = SyncSummary::default();

    let mailbox_dir = staging_dir.join(sink::sanitize_mailbox_path(
        &mailbox_name,
        delimiter.as_deref(),
    ));
    fs::create_dir_all(&mailbox_dir)
        .map_err(|err| format!("failed to create {}: {err}", mailbox_dir.display()))?;

    let mailbox = session
        .examine(&mailbox_name)
        .await
        .map_err(|err| format!("failed to open '{mailbox_name}' read-only: {err}"))?;
    let current_validity = mailbox.uid_validity.unwrap_or(0);

    if sink::is_stale(sink::read_uidvalidity(&mailbox_dir), current_validity) {
        sink::clear_eml_files(&mailbox_dir)?;
        clear_processed(&mailbox_dir)?;
    }
    sink::write_uidvalidity(&mailbox_dir, current_validity)?;

    let server_uids: HashSet<u32> = session
        .uid_search("ALL")
        .await
        .map_err(|err| format!("failed to search '{mailbox_name}': {err}"))?;
    let processed = read_processed(&mailbox_dir)?;
    let pending = sink::missing_uids(&server_uids, &processed);

    if pending.is_empty() {
        println!("{mailbox_name}: up to date ({} processed)", processed.len());
        summary.already_processed += processed.len();
        summary.mailboxes += 1;
        session
            .logout()
            .await
            .map_err(|err| format!("logout failed for '{mailbox_name}': {err}"))?;
        return Ok(summary);
    }

    let on_disk = sink::on_disk_uids(&mailbox_dir)?;
    let pending_set: HashSet<u32> = pending.iter().copied().collect();
    let to_fetch = sink::missing_uids(&pending_set, &on_disk);
    sink::fetch_uids(
        &mut session,
        &mailbox_name,
        &mailbox_dir,
        &to_fetch,
        &multi_progress,
    )
    .await?;

    // ADR-0013: the transform/verify/upload/delete loop below can take as
    // long as (or longer than) the fetch phase above for a large mailbox,
    // so it gets its own progress bar rather than leaving the terminal
    // looking stuck once the fetch bar finishes.
    let sync_bar = sink::new_progress_bar(
        format!("{mailbox_name} sync"),
        pending.len() as u64,
        &multi_progress,
    );

    for uid in &pending {
        sync_bar.inc(1);
        let eml_path = mailbox_dir.join(format!("{uid}.eml"));

        // Both indexes are locked only for this synchronous call
        // (`transform_one` never awaits) and dropped immediately after --
        // never held across an `.await` below.
        let transform_result = {
            let message_guard = message_index.lock().unwrap();
            let attachment_guard = attachment_index.lock().unwrap();
            transform::transform_one(
                &identity,
                &eml_path,
                &staging_dir,
                &output_dir,
                &message_guard,
                &attachment_guard,
            )
        };

        match transform_result? {
            Some(transformed) if transform::verify_transformed(&transformed) => {
                // ADR-0012: an unverified message's content never becomes a
                // dedup target for anything else, so these are committed
                // only now that verification has passed. Each lock is held
                // only for its own short, synchronous `commit()` call; a
                // poisoned lock (from a panicking worker) is allowed to
                // panic here too -- ADR-0014 chose no mid-flight recovery.
                if let Some((hash, relpath)) = &transformed.pending_message_hash {
                    message_index
                        .lock()
                        .unwrap()
                        .commit(&staging_dir, hash, relpath)?;
                }
                for (hash, relpath) in &transformed.pending_attachment_hashes {
                    attachment_index
                        .lock()
                        .unwrap()
                        .commit(&staging_dir, hash, relpath)?;
                }

                if let Some((remote, remote_secret)) = output_remote
                    && should_reupload_after_merge(&transformed)
                {
                    match upload_transformed(remote, remote_secret, &output_dir, &transformed).await
                    {
                        Ok(outcomes) => {
                            for outcome in outcomes {
                                match outcome {
                                    client::UploadOutcome::Uploaded => summary.uploaded += 1,
                                    client::UploadOutcome::Unchanged => summary.unchanged += 1,
                                }
                            }
                        }
                        Err(err) => {
                            eprintln!(
                                "Warning: upload failed for UID {uid} in '{mailbox_name}': {err}, keeping {}",
                                eml_path.display()
                            );
                            summary.upload_failed += 1;
                            continue;
                        }
                    }
                }
                let _ = fs::remove_file(&eml_path);
                append_processed(&mailbox_dir, *uid)?;
                if transformed.merged {
                    summary.merged_messages += 1;
                } else {
                    summary.synced += 1;
                }
                summary.deduped_attachments += transformed.attachments_deduped;
            }
            Some(_) => {
                eprintln!(
                    "Warning: verification failed for UID {uid} in '{mailbox_name}', keeping {}",
                    eml_path.display()
                );
                summary.failed += 1;
            }
            None => {
                summary.failed += 1;
            }
        }
    }
    sync_bar.finish();

    summary.already_processed += processed.len();
    summary.mailboxes += 1;

    session
        .logout()
        .await
        .map_err(|err| format!("logout failed for '{mailbox_name}': {err}"))?;

    Ok(summary)
}

/// Reads the set of UIDs already fetched, transformed, verified, and
/// cleaned up for a mailbox. A missing file (first run) is an empty set.
fn read_processed(mailbox_dir: &Path) -> Result<HashSet<u32>, String> {
    let path = mailbox_dir.join(PROCESSED_FILE_NAME);
    let contents = match fs::read_to_string(&path) {
        Ok(contents) => contents,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(HashSet::new()),
        Err(err) => return Err(format!("failed to read {}: {err}", path.display())),
    };
    Ok(contents
        .lines()
        .filter_map(|line| line.trim().parse().ok())
        .collect())
}

/// Appends `uid` to the `.processed` marker. Append-only and one UID at a
/// time, so a crash mid-run never loses already-recorded progress.
fn append_processed(mailbox_dir: &Path, uid: u32) -> Result<(), String> {
    let path = mailbox_dir.join(PROCESSED_FILE_NAME);
    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .map_err(|err| format!("failed to open {}: {err}", path.display()))?;
    writeln!(file, "{uid}").map_err(|err| format!("failed to write {}: {err}", path.display()))
}

/// The S3 key for `path` (an absolute path rooted at `output_dir`, as
/// produced by `transform::transform_one`): `output_dir`'s relative tree
/// mirrored directly at the bucket root, per ADR-0011. Joined component-wise
/// rather than via `to_string_lossy()` on the whole relative path so the key
/// always uses `/`, regardless of the host platform's path separator.
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

/// Uploads a transformed message's Markdown and every attachment to
/// `remote`, mirroring `output_dir`'s relative tree at the bucket root. Bails
/// out (via `?`) on the first failing file, so a message's upload is treated
/// as atomic -- a partially-uploaded message never gets `.processed`/deleted.
async fn upload_transformed(
    remote: &Remote,
    secret: &str,
    output_dir: &Path,
    transformed: &TransformedMessage,
) -> Result<Vec<client::UploadOutcome>, String> {
    let mut outcomes = Vec::new();
    for path in std::iter::once(&transformed.md_path).chain(transformed.attachment_paths.iter()) {
        let key = upload_key(output_dir, path)?;
        let data =
            fs::read(path).map_err(|err| format!("failed to read {}: {err}", path.display()))?;
        outcomes.push(client::upload_if_changed(remote, secret, &key, data).await?);
    }
    Ok(outcomes)
}

/// Whether a follow-up upload of the canonical `.md` is warranted after this
/// `transform_one` result (ADR-0012 x ADR-0011): always for a fresh
/// message, and for a merge only when the canonical file's frontmatter was
/// actually rewritten -- an idempotent replay of an already-recorded
/// duplicate leaves the file untouched, so re-uploading it would be a
/// pointless (if harmless, since `upload_if_changed` would report
/// `Unchanged`) round trip.
fn should_reupload_after_merge(transformed: &TransformedMessage) -> bool {
    !transformed.merged || transformed.canonical_frontmatter_changed
}

/// Removes the `.processed` marker -- used alongside `sink::clear_eml_files`
/// when a mailbox's `UIDVALIDITY` changes, since old processed-UID records
/// are as meaningless as old raw `.eml`s once that happens.
fn clear_processed(mailbox_dir: &Path) -> Result<(), String> {
    let path = mailbox_dir.join(PROCESSED_FILE_NAME);
    match fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(format!("failed to remove {}: {err}", path.display())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn transformed(merged: bool, canonical_frontmatter_changed: bool) -> TransformedMessage {
        TransformedMessage {
            md_path: PathBuf::from("identity/hello.md"),
            attachment_paths: Vec::new(),
            merged,
            canonical_frontmatter_changed,
            attachments_deduped: 0,
            pending_message_hash: None,
            pending_attachment_hashes: Vec::new(),
        }
    }

    #[test]
    fn sync_summary_merge_sums_every_field() {
        let mut total = SyncSummary {
            mailboxes: 1,
            synced: 2,
            already_processed: 3,
            failed: 4,
            uploaded: 5,
            unchanged: 6,
            upload_failed: 7,
            merged_messages: 8,
            deduped_attachments: 9,
        };
        let other = SyncSummary {
            mailboxes: 10,
            synced: 20,
            already_processed: 30,
            failed: 40,
            uploaded: 50,
            unchanged: 60,
            upload_failed: 70,
            merged_messages: 80,
            deduped_attachments: 90,
        };

        total.merge(&other);

        assert_eq!(total.mailboxes, 11);
        assert_eq!(total.synced, 22);
        assert_eq!(total.already_processed, 33);
        assert_eq!(total.failed, 44);
        assert_eq!(total.uploaded, 55);
        assert_eq!(total.unchanged, 66);
        assert_eq!(total.upload_failed, 77);
        assert_eq!(total.merged_messages, 88);
        assert_eq!(total.deduped_attachments, 99);
    }

    #[test]
    fn should_reupload_after_merge_always_true_for_fresh_message() {
        assert!(should_reupload_after_merge(&transformed(false, false)));
    }

    #[test]
    fn should_reupload_after_merge_true_when_merge_changed_canonical() {
        assert!(should_reupload_after_merge(&transformed(true, true)));
    }

    #[test]
    fn should_reupload_after_merge_false_for_idempotent_merge_replay() {
        assert!(!should_reupload_after_merge(&transformed(true, false)));
    }

    #[test]
    fn read_processed_missing_file_is_empty() {
        let dir = tempfile::tempdir().unwrap();
        assert!(read_processed(dir.path()).unwrap().is_empty());
    }

    #[test]
    fn processed_round_trips_across_appends() {
        let dir = tempfile::tempdir().unwrap();
        append_processed(dir.path(), 5).unwrap();
        append_processed(dir.path(), 9).unwrap();

        assert_eq!(
            read_processed(dir.path()).unwrap(),
            [5, 9].into_iter().collect()
        );
    }

    #[test]
    fn clear_processed_removes_marker() {
        let dir = tempfile::tempdir().unwrap();
        append_processed(dir.path(), 5).unwrap();

        clear_processed(dir.path()).unwrap();

        assert!(read_processed(dir.path()).unwrap().is_empty());
    }

    #[test]
    fn clear_processed_missing_file_is_ok() {
        let dir = tempfile::tempdir().unwrap();
        assert!(clear_processed(dir.path()).is_ok());
    }

    #[test]
    fn upload_key_joins_nested_relative_path_with_forward_slashes() {
        let output_dir = Path::new("/staging/output");
        let path = output_dir.join("identity-at-example.com/attachments/2026-01-01-hi-file.pdf");

        assert_eq!(
            upload_key(output_dir, &path).unwrap(),
            "identity-at-example.com/attachments/2026-01-01-hi-file.pdf"
        );
    }

    #[test]
    fn upload_key_top_level_file_has_no_separator() {
        let output_dir = Path::new("/staging/output");
        let path = output_dir.join("identity-at-example.com/2026-01-01-hi.md");

        assert_eq!(
            upload_key(output_dir, &path).unwrap(),
            "identity-at-example.com/2026-01-01-hi.md"
        );
    }

    #[test]
    fn upload_key_errors_when_path_is_not_under_output_dir() {
        let output_dir = Path::new("/staging/output");
        let path = Path::new("/elsewhere/file.md");

        assert!(upload_key(output_dir, path).is_err());
    }
}
