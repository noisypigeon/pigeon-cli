use std::collections::HashSet;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use async_imap::types::NameAttribute;
use futures::TryStreamExt;
use futures::stream::{self, StreamExt};
use indicatif::MultiProgress;

use crate::dataops::client;
use crate::dataops::dedup::ContentIndex;
use crate::dataops::store::BucketConfig;
use crate::email::identity::{self, Identity};
use crate::email::imap_client;
use crate::email::{sink, transform};

const PROCESSED_FILE_NAME: &str = ".processed";
const UPLOADED_FILE_NAME: &str = ".uploaded";

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
    /// Adds `other`'s counts into `self` -- used both to combine every
    /// concurrent mailbox worker's (ADR-0014) independently-accumulated
    /// summary into one local-phase total, and (ADR-0019) to fold the
    /// separate upload phase's summary into that total.
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
}

/// For every mailbox, for every message not yet in `.processed`: fetch it
/// (unless already staged), transform it, and -- only once the output is
/// verified -- delete the staged `.eml` and record its UID as processed.
/// Per ADR-0007, this is `pigeon email sync`'s default (no `--debug`)
/// behavior. Mailboxes are processed concurrently, up to `concurrency` at a
/// time (ADR-0014).
///
/// Per ADR-0019, the whole local fetch+transform+dedupe phase completes for
/// every mailbox before any upload is attempted -- `output_remote`, if
/// given, is only consulted after `run_local_async` returns, never
/// interleaved with it. This guarantees the upload phase only ever sees
/// fully-deduped, final content.
pub fn run(
    identity: &Identity,
    secret: &str,
    staging_dir: &Path,
    output_dir: &Path,
    output_remote: Option<(&BucketConfig, &str)>,
    concurrency: usize,
) -> Result<SyncSummary, String> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|err| format!("failed to start async runtime: {err}"))?;

    runtime.block_on(async {
        let mut summary =
            run_local_async(identity, secret, staging_dir, output_dir, concurrency).await?;
        if let Some((remote, remote_secret)) = output_remote {
            let upload_summary =
                run_upload_async(identity, output_dir, staging_dir, remote, remote_secret).await?;
            summary.merge(&upload_summary);
        }
        Ok(summary)
    })
}

/// The local fetch+transform+dedupe phase, shared by the default `sync`
/// flow (via `run`, above) and unused directly elsewhere -- `--debug sink`/
/// `--debug transform` have their own simpler, non-concurrent entry points
/// (`sink::run`/`transform::run`), unaffected by this ADR.
async fn run_local_async(
    identity: &Identity,
    secret: &str,
    staging_dir: &Path,
    output_dir: &Path,
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
        transform::MESSAGE_HASHES_FILE,
    )?));
    let attachment_index = Arc::new(Mutex::new(ContentIndex::load(
        staging_dir,
        transform::ATTACHMENT_HASHES_FILE,
    )?));
    let multi_progress = MultiProgress::new();

    let ctx = SyncContext {
        identity: identity.clone(),
        secret: secret.to_string(),
        staging_dir: staging_dir.to_path_buf(),
        output_dir: output_dir.to_path_buf(),
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

/// Fetches, transforms, verifies, and cleans up every pending message in
/// one mailbox, on its own IMAP connection -- ADR-0014's unit of
/// concurrency. `message_index`/`attachment_index` are shared with every
/// other concurrently-running mailbox worker. Per ADR-0019, does not
/// upload -- that happens only after every mailbox worker has finished, in
/// a separate phase.
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
    } = ctx;

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
        // ADR-0015: routed through `MultiProgress` rather than a raw
        // `println!` -- a plain print while other mailboxes' bars are
        // active corrupts their shared redraw state.
        let _ = multi_progress.println(format!(
            "{mailbox_name}: up to date ({} processed)",
            processed.len()
        ));
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

    // ADR-0013: the transform/verify/delete loop below can take as long as
    // (or longer than) the fetch phase above for a large mailbox, so it
    // gets its own progress bar rather than leaving the terminal looking
    // stuck once the fetch bar finishes.
    let sync_bar = sink::new_progress_bar(
        format!("{mailbox_name} sync"),
        pending.len() as u64,
        &multi_progress,
    );

    for uid in &pending {
        sync_bar.inc(1);
        let eml_path = mailbox_dir.join(format!("{uid}.eml"));

        // Both indexes are locked for this synchronous call (`transform_one`
        // never awaits) and dropped immediately after -- never held across
        // an `.await` below. This scope is relied on for two reasons, not
        // just one: ADR-0012's dedup-index consistency (so a concurrent
        // worker's `check()` always sees every commit made so far), *and*
        // ADR-0019's concurrency-safety analysis -- it's what prevents a
        // real TOCTOU race in `unique_path()`'s check-then-write logic
        // against the shared, flat, identity-scoped `output_dir` tree
        // (ADR-0006), since two workers could otherwise resolve the same
        // "free" filename before either has written it. Don't narrow this
        // lock scope without accounting for both. Wrapped in `suspend`
        // (ADR-0015) so `transform_one`'s own internal warning
        // `eprintln!`s -- which `transform.rs` prints directly, with no
        // `MultiProgress` of its own -- don't corrupt the active bars'
        // redraw state either.
        let transform_result = multi_progress.suspend(|| {
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
        });

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
                let _ = multi_progress.println(format!(
                    "Warning: verification failed for UID {uid} in '{mailbox_name}', keeping {}",
                    eml_path.display()
                ));
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
/// Per ADR-0019, this means "done with local fetch+transform+dedupe," not
/// "fully done including upload" -- `.uploaded` (below) is the separate
/// source of truth for upload progress.
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

/// Uploads every file under `identity`'s `output_dir` subtree that isn't
/// already recorded in `.uploaded`, per ADR-0019. Runs only after
/// `run_local_async` has fully completed for the whole identity (either as
/// part of `run`, or standalone via `--debug upload`/`run_upload`), so
/// every file it sees is already in its final, deduped state -- there's no
/// "reupload after a later merge" case to handle (ADR-0012's mechanism for
/// that is removed by this ADR).
async fn run_upload_async(
    identity: &Identity,
    output_dir: &Path,
    staging_dir: &Path,
    remote: &BucketConfig,
    remote_secret: &str,
) -> Result<SyncSummary, String> {
    let identity_dir = output_dir.join(identity::sanitize_segment(&identity.email));
    let mut uploaded_index = UploadedIndex::load(staging_dir)?;
    let files = crate::dataops::transform::collect_files(&identity_dir)?;

    let mut summary = SyncSummary::default();
    for path in files {
        let key = upload_key(output_dir, &path)?;
        if uploaded_index.contains(&key) {
            continue;
        }

        let data =
            fs::read(&path).map_err(|err| format!("failed to read {}: {err}", path.display()))?;
        match client::upload_if_changed(remote, remote_secret, &key, data).await {
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

/// Standalone entry point for `--debug upload` (ADR-0019): uploads the
/// existing local `output_dir` tree to `remote`, skipping fetch/transform
/// entirely. Needs no IMAP session or credentials at all -- matches
/// `sink::run`/`transform::run`'s own-runtime, standalone-phase shape.
pub fn run_upload(
    identity: &Identity,
    staging_dir: &Path,
    output_dir: &Path,
    remote: &BucketConfig,
    remote_secret: &str,
) -> Result<SyncSummary, String> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|err| format!("failed to start async runtime: {err}"))?;

    runtime.block_on(run_upload_async(
        identity,
        output_dir,
        staging_dir,
        remote,
        remote_secret,
    ))
}

/// Tracks which output files (by their S3 key, per `upload_key`) have
/// already been confirmed uploaded, so a resumed/re-run upload phase can
/// skip them without a redundant network round-trip. Purely a
/// resumability-speed optimization, not a correctness requirement --
/// `client::upload_if_changed`'s ETag comparison is already idempotent on
/// its own. `.uploaded` lives at `staging_dir`'s root, append-only, one key
/// per line -- the same shape as `dedup::ContentIndex`'s dotfiles, but a
/// plain set rather than a hash-to-path map, since no content hash is
/// needed here.
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

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn uploaded_index_load_missing_file_is_empty() {
        let dir = tempfile::tempdir().unwrap();
        let index = UploadedIndex::load(dir.path()).unwrap();
        assert!(!index.contains("identity/hello.md"));
    }

    #[test]
    fn uploaded_index_commit_then_contains_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let mut index = UploadedIndex::load(dir.path()).unwrap();

        index.commit(dir.path(), "identity/hello.md").unwrap();

        assert!(index.contains("identity/hello.md"));
        assert!(!index.contains("identity/other.md"));

        // Reloading from disk picks up the committed entry too.
        let reloaded = UploadedIndex::load(dir.path()).unwrap();
        assert!(reloaded.contains("identity/hello.md"));
    }
}
