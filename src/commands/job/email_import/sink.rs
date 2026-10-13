//! The IMAP fetch loop and the per-mailbox bookkeeping around it
//! (ADR-0147 §0).
//!
//! Relocated from `email_sync/sink.rs` when ADR-0147 deleted that job. Six
//! functions moved; `new_progress_bar` went to `commands::job::progress`
//! instead, having two consumers rather than one, and `sink::run`,
//! `SinkSummary`, `on_disk_uids`, and `clear_eml_files` were deleted
//! outright -- the first two being the dead `pigeon email sync --debug
//! sink` entry point from ADR-0007 §4, the latter two reasoning about a
//! local `.eml` tree this job never keeps (ADR-0146 §5's GC deletes each
//! message as soon as its push resolves).
//!
//! Everything here predates `email import`, so the ADR citations in each
//! doc comment are the originals, re-homed rather than rewritten.

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

use futures::TryStreamExt;
use indicatif::MultiProgress;

use crate::commands::job::progress::new_progress_bar;
use crate::commands::keyring::email::identity::sanitize_segment;
use crate::commands::keyring::email::imap_client::ImapSession;

const UIDVALIDITY_FILE_NAME: &str = ".uidvalidity";

/// Fetches every UID in `missing` from `mailbox_name` (already `EXAMINE`d on
/// `session`) via `BODY.PEEK[]` and writes each as `<mailbox_dir>/<uid>.eml`.
/// Returns the count written.
///
/// `EXAMINE` plus `BODY.PEEK[]` is what makes the whole read path
/// non-mutating server-side (ADR-0005): nothing is marked `\Seen` and no
/// flag is touched.
pub(crate) async fn fetch_uids(
    session: &mut ImapSession,
    mailbox_name: &str,
    mailbox_dir: &Path,
    missing: &[u32],
    multi_progress: &MultiProgress,
) -> Result<usize, String> {
    if missing.is_empty() {
        return Ok(0);
    }

    let bar = new_progress_bar(
        format!("{mailbox_name} fetch"),
        missing.len() as u64,
        multi_progress,
    );

    let uid_set = missing
        .iter()
        .map(|uid| uid.to_string())
        .collect::<Vec<_>>()
        .join(",");

    let mut fetches = session
        .uid_fetch(&uid_set, "(UID BODY.PEEK[])")
        .await
        .map_err(|err| format!("failed to fetch messages in '{mailbox_name}': {err}"))?;

    let mut written = 0;
    while let Some(fetch) = fetches
        .try_next()
        .await
        .map_err(|err| format!("failed to fetch messages in '{mailbox_name}': {err}"))?
    {
        let (Some(uid), Some(body)) = (fetch.uid, fetch.body()) else {
            // The caller turns a UID with no file on disk afterwards into a
            // counted `missing_file` failure (ADR-0146 §8).
            tracing::warn!(
                mailbox = mailbox_name,
                uid = fetch.uid,
                "dropped FETCH response missing uid or body"
            );
            continue;
        };
        let path = mailbox_dir.join(format!("{uid}.eml"));
        fs::write(&path, body)
            .map_err(|err| format!("failed to write {}: {err}", path.display()))?;
        written += 1;
        bar.inc(1);
    }
    bar.finish();

    Ok(written)
}

/// Splits `name` on `delimiter` (treating the whole name as one segment when
/// there is none) and sanitizes each segment into a filesystem-safe path,
/// e.g. `Archive/2020` with delimiter `/` -> `archive/2020`.
pub(crate) fn sanitize_mailbox_path(name: &str, delimiter: Option<&str>) -> PathBuf {
    let segments: Vec<&str> = match delimiter {
        Some(delimiter) if !delimiter.is_empty() => name.split(delimiter).collect(),
        _ => vec![name],
    };
    segments
        .into_iter()
        .map(sanitize_segment)
        .filter(|segment| !segment.is_empty())
        .collect()
}

/// The sorted set of UIDs present in `server` but not in `local`. A generic
/// set difference with no IMAP-specific semantics -- `gather_pending` uses
/// it to compute "every UID the server reports, minus every one already
/// checkpointed as delivered".
pub(crate) fn missing_uids(server: &HashSet<u32>, local: &HashSet<u32>) -> Vec<u32> {
    let mut missing: Vec<u32> = server.difference(local).copied().collect();
    missing.sort_unstable();
    missing
}

/// A mailbox is stale when it has a recorded `UIDVALIDITY` that no longer
/// matches the server's current one. A missing marker (first run) is never
/// stale -- there's nothing to compare against yet.
pub(crate) fn is_stale(on_disk_validity: Option<u32>, current_validity: u32) -> bool {
    on_disk_validity.is_some_and(|value| value != current_validity)
}

pub(crate) fn read_uidvalidity(mailbox_dir: &Path) -> Option<u32> {
    fs::read_to_string(mailbox_dir.join(UIDVALIDITY_FILE_NAME))
        .ok()?
        .trim()
        .parse()
        .ok()
}

pub(crate) fn write_uidvalidity(mailbox_dir: &Path, value: u32) -> Result<(), String> {
    let path = mailbox_dir.join(UIDVALIDITY_FILE_NAME);
    fs::write(&path, value.to_string())
        .map_err(|err| format!("failed to write {}: {err}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitizes_single_segment_mailbox() {
        assert_eq!(sanitize_mailbox_path("INBOX", None), PathBuf::from("inbox"));
    }

    #[test]
    fn sanitizes_multi_segment_mailbox() {
        assert_eq!(
            sanitize_mailbox_path("Archive/2020", Some("/")),
            PathBuf::from("archive").join("2020")
        );
    }

    #[test]
    fn sanitizes_spaces_in_mailbox_name() {
        assert_eq!(
            sanitize_mailbox_path("Sent Items", None),
            PathBuf::from("sent-items")
        );
    }

    #[test]
    fn missing_uids_is_server_minus_local() {
        let server: HashSet<u32> = [1, 2, 3, 4, 5].into_iter().collect();
        let local: HashSet<u32> = [1, 2, 3].into_iter().collect();
        assert_eq!(missing_uids(&server, &local), vec![4, 5]);
    }

    #[test]
    fn missing_uids_empty_when_up_to_date() {
        let server: HashSet<u32> = [1, 2].into_iter().collect();
        let local: HashSet<u32> = [1, 2].into_iter().collect();
        assert!(missing_uids(&server, &local).is_empty());
    }

    #[test]
    fn first_run_is_never_stale() {
        assert!(!is_stale(None, 100));
    }

    #[test]
    fn matching_validity_is_not_stale() {
        assert!(!is_stale(Some(100), 100));
    }

    #[test]
    fn mismatched_validity_is_stale() {
        assert!(is_stale(Some(100), 200));
    }

    #[test]
    fn uidvalidity_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(read_uidvalidity(dir.path()), None);

        write_uidvalidity(dir.path(), 42).unwrap();
        assert_eq!(read_uidvalidity(dir.path()), Some(42));
    }
}
