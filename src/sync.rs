use std::collections::HashSet;
use std::fs;
use std::io::Write;
use std::path::Path;

use async_imap::types::NameAttribute;
use futures::TryStreamExt;

use crate::identity::Identity;
use crate::imap_client;
use crate::{sink, transform};

const PROCESSED_FILE_NAME: &str = ".processed";

/// Summary of a completed `sync` run.
#[derive(Debug, Default)]
pub struct SyncSummary {
    pub mailboxes: usize,
    pub synced: usize,
    pub already_processed: usize,
    pub failed: usize,
}

/// For every mailbox, for every message not yet in `.processed`: fetch it
/// (unless already staged), transform it, and -- only once the output is
/// verified -- delete the staged `.eml` and record its UID as processed.
/// Per ADR-0007, this is `pigeon email sync`'s default (no `--debug`)
/// behavior.
pub fn run(
    identity: &Identity,
    secret: &str,
    staging_dir: &Path,
    output_dir: &Path,
) -> Result<SyncSummary, String> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .build()
        .map_err(|err| format!("failed to start async runtime: {err}"))?;

    runtime.block_on(run_async(identity, secret, staging_dir, output_dir))
}

async fn run_async(
    identity: &Identity,
    secret: &str,
    staging_dir: &Path,
    output_dir: &Path,
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

    let mut summary = SyncSummary::default();

    for name in &names {
        if name.attributes().contains(&NameAttribute::NoSelect) {
            continue;
        }

        let mailbox_name = name.name();
        let mailbox_dir =
            staging_dir.join(sink::sanitize_mailbox_path(mailbox_name, name.delimiter()));
        fs::create_dir_all(&mailbox_dir)
            .map_err(|err| format!("failed to create {}: {err}", mailbox_dir.display()))?;

        let mailbox = session
            .examine(mailbox_name)
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
            summary.already_processed += processed.len();
            summary.mailboxes += 1;
            continue;
        }

        let on_disk = sink::on_disk_uids(&mailbox_dir)?;
        let pending_set: HashSet<u32> = pending.iter().copied().collect();
        let to_fetch = sink::missing_uids(&pending_set, &on_disk);
        sink::fetch_uids(&mut session, mailbox_name, &mailbox_dir, &to_fetch).await?;

        for uid in &pending {
            let eml_path = mailbox_dir.join(format!("{uid}.eml"));
            match transform::transform_one(identity, &eml_path, staging_dir, output_dir)? {
                Some(transformed) if transform::verify_transformed(&transformed) => {
                    let _ = fs::remove_file(&eml_path);
                    append_processed(&mailbox_dir, *uid)?;
                    summary.synced += 1;
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

        summary.already_processed += processed.len();
        summary.mailboxes += 1;
    }

    session
        .logout()
        .await
        .map_err(|err| format!("logout failed: {err}"))?;

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
}
