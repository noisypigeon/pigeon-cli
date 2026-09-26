use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

use async_imap::types::NameAttribute;
use futures::TryStreamExt;
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};

use crate::commands::keyring::email::identity::sanitize_segment;
use crate::commands::keyring::email::imap_client::{self, ImapSession};

const UIDVALIDITY_FILE_NAME: &str = ".uidvalidity";

/// Summary of a completed `sink` run.
#[derive(Debug, Default)]
pub struct SinkSummary {
    pub mailboxes: usize,
    pub downloaded: usize,
    pub already_present: usize,
}

/// Downloads every message in every mailbox for one identity into
/// `directory`, one `.eml` file per message, without mutating anything
/// server-side (`EXAMINE` + `BODY.PEEK[]`, per ADR-0005). Safe to re-run:
/// already-downloaded messages are skipped.
///
/// This is `pigeon email sync --debug sink`'s implementation (ADR-0007) --
/// fetch-only, never transforms, never deletes.
pub fn run(
    email: &str,
    host: &str,
    port: u16,
    secret: &str,
    accept_invalid_certs: bool,
    directory: &Path,
) -> Result<SinkSummary, String> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|err| format!("failed to start async runtime: {err}"))?;

    runtime.block_on(run_async(
        email,
        host,
        port,
        secret,
        accept_invalid_certs,
        directory,
    ))
}

async fn run_async(
    email: &str,
    host: &str,
    port: u16,
    secret: &str,
    accept_invalid_certs: bool,
    directory: &Path,
) -> Result<SinkSummary, String> {
    let mut session =
        imap_client::connect_and_login(host, port, email, secret, accept_invalid_certs).await?;

    let names: Vec<_> = session
        .list(None, Some("*"))
        .await
        .map_err(|err| format!("failed to list mailboxes: {err}"))?
        .try_collect()
        .await
        .map_err(|err| format!("failed to list mailboxes: {err}"))?;

    let mut summary = SinkSummary::default();
    // `--debug sink` stays sequential (ADR-0014), so a single-slot
    // `MultiProgress` renders identically to a bare bar -- this just lets it
    // share `new_progress_bar`/`fetch_uids` with the concurrent `sync` path.
    let multi_progress = MultiProgress::new();

    for name in &names {
        if name.attributes().contains(&NameAttribute::NoSelect) {
            continue;
        }

        let mailbox_name = name.name();
        let mailbox_dir = directory.join(sanitize_mailbox_path(mailbox_name, name.delimiter()));
        fs::create_dir_all(&mailbox_dir)
            .map_err(|err| format!("failed to create {}: {err}", mailbox_dir.display()))?;

        let mailbox = session
            .examine(mailbox_name)
            .await
            .map_err(|err| format!("failed to open '{mailbox_name}' read-only: {err}"))?;
        let current_validity = mailbox.uid_validity.unwrap_or(0);

        if is_stale(read_uidvalidity(&mailbox_dir), current_validity) {
            clear_eml_files(&mailbox_dir)?;
        }
        write_uidvalidity(&mailbox_dir, current_validity)?;

        let server_uids: HashSet<u32> = session
            .uid_search("ALL")
            .await
            .map_err(|err| format!("failed to search '{mailbox_name}': {err}"))?;
        let local_uids = on_disk_uids(&mailbox_dir)?;
        let missing = missing_uids(&server_uids, &local_uids);

        summary.downloaded += fetch_uids(
            &mut session,
            mailbox_name,
            &mailbox_dir,
            &missing,
            &multi_progress,
        )
        .await?;
        summary.already_present += local_uids.len();
        summary.mailboxes += 1;
    }

    session
        .logout()
        .await
        .map_err(|err| format!("logout failed: {err}"))?;

    Ok(summary)
}

/// Builds a `ProgressBar` with the style shared by every phase of a
/// `sync`/`sink` run (fetch, and -- per ADR-0013 -- transform/upload), so
/// they read consistently in scrollback: `{prefix} {bar:40} {pos}/{len}`.
/// Registered on `multi_progress` so concurrent `sync` workers (ADR-0014)
/// render as simultaneous lines rather than overwriting each other.
pub(crate) fn new_progress_bar(
    prefix: String,
    len: u64,
    multi_progress: &MultiProgress,
) -> ProgressBar {
    let bar = ProgressBar::new(len);
    if let Ok(style) = ProgressStyle::with_template("{prefix:24!} {bar:40} {pos}/{len}") {
        bar.set_style(style);
    }
    bar.set_prefix(prefix);
    multi_progress.add(bar)
}

/// Fetches every UID in `missing` from `mailbox_name` (already `EXAMINE`d on
/// `session`) via `BODY.PEEK[]` and writes each as `<mailbox_dir>/<uid>.eml`.
/// Returns the count written. Shared by `sink::run` and `sync::run`
/// (ADR-0007) -- the only piece of sink's behavior `sync` reuses directly;
/// each caller computes its own `missing` set according to its own resume
/// semantics.
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

/// Scans `mailbox_dir` for `<uid>.eml` files, parsing filenames as UIDs.
/// Anything that doesn't parse is silently skipped, not an error.
pub(crate) fn on_disk_uids(mailbox_dir: &Path) -> Result<HashSet<u32>, String> {
    let mut uids = HashSet::new();
    let entries = match fs::read_dir(mailbox_dir) {
        Ok(entries) => entries,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(uids),
        Err(err) => {
            return Err(format!("failed to read {}: {err}", mailbox_dir.display()));
        }
    };
    for entry in entries {
        let entry =
            entry.map_err(|err| format!("failed to read {}: {err}", mailbox_dir.display()))?;
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("eml") {
            continue;
        }
        if let Some(stem) = path.file_stem().and_then(|stem| stem.to_str())
            && let Ok(uid) = stem.parse::<u32>()
        {
            uids.insert(uid);
        }
    }
    Ok(uids)
}

/// The sorted set of UIDs present in `server` but not in `local`. Generic
/// set-difference with no sink-specific semantics -- reused by `sync`
/// (ADR-0007) to compute pending (server minus already-processed) UIDs too.
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

/// Removes every `<uid>.eml` file in `mailbox_dir` -- used when its
/// `UIDVALIDITY` no longer matches the server's, so no stale UID/message
/// pairing survives.
pub(crate) fn clear_eml_files(mailbox_dir: &Path) -> Result<(), String> {
    let entries = match fs::read_dir(mailbox_dir) {
        Ok(entries) => entries,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(err) => {
            return Err(format!("failed to read {}: {err}", mailbox_dir.display()));
        }
    };
    for entry in entries {
        let entry =
            entry.map_err(|err| format!("failed to read {}: {err}", mailbox_dir.display()))?;
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) == Some("eml") {
            fs::remove_file(&path)
                .map_err(|err| format!("failed to remove {}: {err}", path.display()))?;
        }
    }
    Ok(())
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
    fn on_disk_uids_parses_eml_filenames_and_skips_others() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("5.eml"), b"").unwrap();
        fs::write(dir.path().join("9.eml"), b"").unwrap();
        fs::write(dir.path().join("not-a-uid.eml"), b"").unwrap();
        fs::write(dir.path().join(".uidvalidity"), b"1").unwrap();

        let uids = on_disk_uids(dir.path()).unwrap();
        assert_eq!(uids, [5, 9].into_iter().collect());
    }

    #[test]
    fn on_disk_uids_missing_directory_is_empty() {
        let dir = tempfile::tempdir().unwrap();
        let missing_dir = dir.path().join("does-not-exist");
        assert!(on_disk_uids(&missing_dir).unwrap().is_empty());
    }

    #[test]
    fn uidvalidity_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(read_uidvalidity(dir.path()), None);

        write_uidvalidity(dir.path(), 42).unwrap();
        assert_eq!(read_uidvalidity(dir.path()), Some(42));
    }

    #[test]
    fn clear_eml_files_removes_only_eml_files() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("5.eml"), b"").unwrap();
        fs::write(dir.path().join(".uidvalidity"), b"1").unwrap();

        clear_eml_files(dir.path()).unwrap();

        assert!(!dir.path().join("5.eml").exists());
        assert!(dir.path().join(".uidvalidity").exists());
    }
}
