use std::collections::HashSet;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use futures::TryStreamExt;

use crate::commands::keyring::email::imap_client::ImapSession;

const MANIFEST_FILE_NAME: &str = ".manifest";
const CHECKPOINT_FILE_NAME: &str = ".job-checkpoint";

/// One pending `(mailbox, UID)` pair discovered by `pull_manifest`, with its
/// byte size (from `RFC822.SIZE`, never the message body itself) -- feeds
/// the wizard's summary display and concurrency-estimate heuristic
/// (ADR-0021 §3/§9). `mailbox` is the raw IMAP mailbox name (as passed to
/// `EXAMINE`/`UID FETCH`), not a sanitized filesystem path -- that
/// conversion (`email::sink::sanitize_mailbox_path`) happens only where a
/// path is actually needed, so manifest/checkpoint entries can be matched
/// against each other without a lossy round-trip through sanitization.
pub(crate) struct ManifestEntry {
    pub mailbox: String,
    pub uid: u32,
    pub size: u64,
}

/// Pulls size metadata for every UID in `pending` from `mailbox_name`
/// (already `EXAMINE`d on `session`), via `UID FETCH ... (UID RFC822.SIZE)`
/// -- the same `uid_fetch` mechanism `email::sink::fetch_uids` uses, with a
/// different data-item list that never transfers message content (per
/// ADR-0021 §3).
pub(crate) async fn pull_manifest(
    session: &mut ImapSession,
    mailbox_name: &str,
    pending: &[u32],
) -> Result<Vec<ManifestEntry>, String> {
    if pending.is_empty() {
        return Ok(Vec::new());
    }

    let uid_set = pending
        .iter()
        .map(|uid| uid.to_string())
        .collect::<Vec<_>>()
        .join(",");

    let mut fetches = session
        .uid_fetch(&uid_set, "(UID RFC822.SIZE)")
        .await
        .map_err(|err| format!("failed to fetch message sizes in '{mailbox_name}': {err}"))?;

    let mut entries = Vec::new();
    while let Some(fetch) = fetches
        .try_next()
        .await
        .map_err(|err| format!("failed to fetch message sizes in '{mailbox_name}': {err}"))?
    {
        let (Some(uid), Some(size)) = (fetch.uid, fetch.size) else {
            continue;
        };
        entries.push(ManifestEntry {
            mailbox: mailbox_name.to_string(),
            uid,
            size: u64::from(size),
        });
    }

    Ok(entries)
}

/// Persists `entries` as a full snapshot to `staging_dir/.manifest` -- a
/// fresh pull wholesale replaces whatever was there before, unlike the
/// append-only checkpoint/dedup dotfiles elsewhere in this codebase.
/// Tab-separated, since mailbox names can legitimately contain spaces.
pub(crate) fn save_manifest(staging_dir: &Path, entries: &[ManifestEntry]) -> Result<(), String> {
    let path = staging_dir.join(MANIFEST_FILE_NAME);
    let mut contents = String::new();
    for entry in entries {
        contents.push_str(&format!(
            "{}\t{}\t{}\n",
            entry.mailbox, entry.uid, entry.size
        ));
    }
    fs::write(&path, contents).map_err(|err| format!("failed to write {}: {err}", path.display()))
}

/// Loads a previously persisted manifest. A missing file (never pulled, or
/// cleared by a `.uidvalidity` staleness reset) is an empty manifest.
/// Malformed lines are skipped leniently, matching every other loader in
/// this codebase.
pub(crate) fn load_manifest(staging_dir: &Path) -> Result<Vec<ManifestEntry>, String> {
    let path = staging_dir.join(MANIFEST_FILE_NAME);
    let contents = match fs::read_to_string(&path) {
        Ok(contents) => contents,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => return Err(format!("failed to read {}: {err}", path.display())),
    };
    Ok(contents
        .lines()
        .filter_map(|line| {
            let mut parts = line.split('\t');
            let mailbox = parts.next()?.to_string();
            let uid = parts.next()?.parse().ok()?;
            let size = parts.next()?.parse().ok()?;
            Some(ManifestEntry { mailbox, uid, size })
        })
        .collect())
}

/// One fully fetched, transformed, and verified `(mailbox, UID)`, recorded
/// in `.job-checkpoint` -- replaces `.processed`'s role (ADR-0007/0019),
/// spanning every mailbox for an identity in one file instead of one file
/// per mailbox, since batches (§4 below) no longer respect mailbox
/// boundaries. Carries everything the post-transform dedup pass (ADR-0021
/// §7) needs without re-reading or re-hashing anything: the message's raw
/// content hash, where its `.md` was staged, and each attachment's hash and
/// staged location.
pub(crate) struct CheckpointEntry {
    /// Raw IMAP mailbox name -- matches `ManifestEntry::mailbox` for
    /// `done_uids` membership checks.
    pub mailbox: String,
    pub uid: u32,
    pub message_hash: String,
    pub md_staged_relpath: String,
    /// The human-readable, date+subject-derived filename this message
    /// would be named at its final, identity-rooted location (ADR-0006),
    /// e.g. `2024-01-26-hello-world.md`
    /// (`email::transform::TransformOutcome::desired_md_name`) -- carried
    /// through so the dedup pass's `unique_path` placement call doesn't
    /// need to re-derive it from the message's own content.
    pub desired_md_name: String,
    /// The `mailbox/...` frontmatter tag this message was staged with
    /// (`email::transform::TransformOutcome::mailbox_tag`) -- distinct from
    /// `mailbox` above; needed by the dedup pass's
    /// `amend_frontmatter_for_duplicate` call, which can't cheaply re-derive
    /// a sanitized tag from a raw IMAP name without that mailbox's
    /// delimiter on hand.
    pub mailbox_tag: String,
    pub attachments: Vec<(String, String)>,
}

/// Appends one verified entry to `staging_dir/.job-checkpoint`. Append-only,
/// one entry at a time, so a crash mid-run never loses already-recorded
/// progress -- same discipline as `.processed`/`.uploaded`/the dedup
/// dotfiles elsewhere in this codebase. Format: `<mailbox>\t<uid>\t
/// <message-hash>\t<md-staged-relpath>\t<desired-md-name>\t<mailbox-tag>\t
/// <hash>=<relpath>[;<hash>=<relpath>...]` (the last field is empty for a
/// message with no attachments).
pub(crate) fn append_checkpoint(staging_dir: &Path, entry: &CheckpointEntry) -> Result<(), String> {
    let path = staging_dir.join(CHECKPOINT_FILE_NAME);
    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .map_err(|err| format!("failed to open {}: {err}", path.display()))?;
    writeln!(file, "{}", format_checkpoint_line(entry))
        .map_err(|err| format!("failed to write {}: {err}", path.display()))
}

fn format_checkpoint_line(entry: &CheckpointEntry) -> String {
    let attachments = entry
        .attachments
        .iter()
        .map(|(hash, relpath)| format!("{hash}={relpath}"))
        .collect::<Vec<_>>()
        .join(";");
    format!(
        "{}\t{}\t{}\t{}\t{}\t{}\t{}",
        entry.mailbox,
        entry.uid,
        entry.message_hash,
        entry.md_staged_relpath,
        entry.desired_md_name,
        entry.mailbox_tag,
        attachments
    )
}

/// Rewrites `.job-checkpoint`, dropping every entry for `mailbox` -- used
/// when that mailbox's `UIDVALIDITY` no longer matches the server's (a
/// recreated mailbox), so stale UID/message pairings don't linger forever
/// as false "already done" markers. Mirrors `sink::clear_eml_files`'s
/// per-mailbox reset, just filtering a shared, identity-wide file instead
/// of removing a per-mailbox one.
pub(crate) fn clear_checkpoint_for_mailbox(
    staging_dir: &Path,
    mailbox: &str,
) -> Result<(), String> {
    let entries = load_checkpoint(staging_dir)?;
    let contents = entries
        .iter()
        .filter(|entry| entry.mailbox != mailbox)
        .map(|entry| format!("{}\n", format_checkpoint_line(entry)))
        .collect::<String>();
    let path = staging_dir.join(CHECKPOINT_FILE_NAME);
    fs::write(&path, contents).map_err(|err| format!("failed to write {}: {err}", path.display()))
}

/// Loads every entry recorded in `staging_dir/.job-checkpoint`. A missing
/// file (first run) is an empty list. Malformed lines are skipped
/// leniently, matching every other loader in this codebase.
pub(crate) fn load_checkpoint(staging_dir: &Path) -> Result<Vec<CheckpointEntry>, String> {
    let path = staging_dir.join(CHECKPOINT_FILE_NAME);
    let contents = match fs::read_to_string(&path) {
        Ok(contents) => contents,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => return Err(format!("failed to read {}: {err}", path.display())),
    };
    Ok(contents
        .lines()
        .filter_map(|line| {
            let mut parts = line.split('\t');
            let mailbox = parts.next()?.to_string();
            let uid = parts.next()?.parse().ok()?;
            let message_hash = parts.next()?.to_string();
            let md_staged_relpath = parts.next()?.to_string();
            let desired_md_name = parts.next()?.to_string();
            let mailbox_tag = parts.next()?.to_string();
            let attachments_field = parts.next()?;
            let attachments = if attachments_field.is_empty() {
                Vec::new()
            } else {
                attachments_field
                    .split(';')
                    .filter_map(|pair| pair.split_once('='))
                    .map(|(hash, relpath)| (hash.to_string(), relpath.to_string()))
                    .collect()
            };
            Some(CheckpointEntry {
                mailbox,
                uid,
                message_hash,
                md_staged_relpath,
                desired_md_name,
                mailbox_tag,
                attachments,
            })
        })
        .collect())
}

/// The set of `(mailbox, UID)` pairs already recorded as done -- an O(1)
/// membership check for deciding what's still pending, mirroring
/// `.processed`'s `HashSet<u32>` role but composite-keyed since one
/// checkpoint file now spans every mailbox.
pub(crate) fn done_uids(entries: &[CheckpointEntry]) -> HashSet<(String, u32)> {
    entries
        .iter()
        .map(|entry| (entry.mailbox.clone(), entry.uid))
        .collect()
}

/// One unit of concurrent fetch+transform work (ADR-0021 §6): a contiguous
/// slice of one mailbox's pending UIDs, small enough that batch count
/// comfortably exceeds `concurrency`. `mailbox` is the raw IMAP name (for
/// `EXAMINE`/`UID FETCH`); `mailbox_relpath` is its sanitized filesystem
/// path (`email::sink::sanitize_mailbox_path`), computed once by the
/// caller rather than re-derived per batch.
pub(crate) struct Batch {
    pub mailbox: String,
    pub mailbox_relpath: PathBuf,
    pub uids: Vec<u32>,
}

/// Batches are sized so at least this many exist per worker, so a mailbox
/// with thousands of pending messages keeps every worker busy instead of
/// finishing in one chunk per mailbox -- the concurrency-capped-by-mailbox-
/// count bug this ADR fixes (Context problem 1).
const MIN_BATCHES_PER_WORKER: usize = 4;

/// Splits `uids` into roughly-equal chunks. Order is preserved (callers
/// that want deterministic canonical-occurrence selection sort `uids`
/// before splitting), and every UID appears in exactly one output chunk.
pub(crate) fn split_into_batches(uids: &[u32], concurrency: usize) -> Vec<Vec<u32>> {
    if uids.is_empty() {
        return Vec::new();
    }
    let batch_size = (uids.len() / (concurrency.max(1) * MIN_BATCHES_PER_WORKER)).max(1);
    uids.chunks(batch_size).map(<[u32]>::to_vec).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn load_missing_manifest_is_empty() {
        let dir = tempfile::tempdir().unwrap();
        assert!(load_manifest(dir.path()).unwrap().is_empty());
    }

    #[test]
    fn manifest_round_trips_through_save_and_load() {
        let dir = tempfile::tempdir().unwrap();
        let entries = vec![
            ManifestEntry {
                mailbox: "INBOX".to_string(),
                uid: 5,
                size: 1024,
            },
            ManifestEntry {
                mailbox: "Sent Items".to_string(),
                uid: 9,
                size: 2048,
            },
        ];

        save_manifest(dir.path(), &entries).unwrap();
        let loaded = load_manifest(dir.path()).unwrap();

        assert_eq!(loaded.len(), 2);
        assert_eq!(loaded[0].mailbox, "INBOX");
        assert_eq!(loaded[0].uid, 5);
        assert_eq!(loaded[0].size, 1024);
        assert_eq!(loaded[1].mailbox, "Sent Items");
        assert_eq!(loaded[1].uid, 9);
        assert_eq!(loaded[1].size, 2048);
    }

    #[test]
    fn save_manifest_is_a_full_snapshot_not_an_append() {
        let dir = tempfile::tempdir().unwrap();
        save_manifest(
            dir.path(),
            &[ManifestEntry {
                mailbox: "INBOX".to_string(),
                uid: 1,
                size: 10,
            }],
        )
        .unwrap();
        save_manifest(
            dir.path(),
            &[ManifestEntry {
                mailbox: "INBOX".to_string(),
                uid: 2,
                size: 20,
            }],
        )
        .unwrap();

        let loaded = load_manifest(dir.path()).unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].uid, 2);
    }

    #[test]
    fn load_manifest_skips_malformed_lines() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join(MANIFEST_FILE_NAME),
            "INBOX\t5\t1024\nnot-enough-fields\nINBOX\t6\t2048\n",
        )
        .unwrap();

        let loaded = load_manifest(dir.path()).unwrap();
        assert_eq!(loaded.len(), 2);
        assert_eq!(loaded[0].uid, 5);
        assert_eq!(loaded[1].uid, 6);
    }

    #[test]
    fn load_missing_checkpoint_is_empty() {
        let dir = tempfile::tempdir().unwrap();
        assert!(load_checkpoint(dir.path()).unwrap().is_empty());
    }

    #[test]
    fn checkpoint_round_trips_across_appends() {
        let dir = tempfile::tempdir().unwrap();
        append_checkpoint(
            dir.path(),
            &CheckpointEntry {
                mailbox: "INBOX".to_string(),
                uid: 5,
                message_hash: "abc123".to_string(),
                md_staged_relpath: "inbox/5.md".to_string(),
                desired_md_name: "2024-01-26-hello.md".to_string(),
                mailbox_tag: "mailbox/inbox".to_string(),
                attachments: Vec::new(),
            },
        )
        .unwrap();
        append_checkpoint(
            dir.path(),
            &CheckpointEntry {
                mailbox: "INBOX".to_string(),
                uid: 9,
                message_hash: "def456".to_string(),
                md_staged_relpath: "inbox/9.md".to_string(),
                desired_md_name: "2024-01-27-world.md".to_string(),
                mailbox_tag: "mailbox/inbox".to_string(),
                attachments: vec![
                    ("hash1".to_string(), "inbox/9/attachments/a.pdf".to_string()),
                    ("hash2".to_string(), "inbox/9/attachments/b.png".to_string()),
                ],
            },
        )
        .unwrap();

        let loaded = load_checkpoint(dir.path()).unwrap();
        assert_eq!(loaded.len(), 2);
        assert_eq!(loaded[0].uid, 5);
        assert!(loaded[0].attachments.is_empty());
        assert_eq!(loaded[1].uid, 9);
        assert_eq!(loaded[1].message_hash, "def456");
        assert_eq!(loaded[1].md_staged_relpath, "inbox/9.md");
        assert_eq!(loaded[1].mailbox_tag, "mailbox/inbox");
        assert_eq!(
            loaded[1].attachments,
            vec![
                ("hash1".to_string(), "inbox/9/attachments/a.pdf".to_string()),
                ("hash2".to_string(), "inbox/9/attachments/b.png".to_string()),
            ]
        );
    }

    #[test]
    fn clear_checkpoint_for_mailbox_drops_only_matching_entries() {
        let dir = tempfile::tempdir().unwrap();
        append_checkpoint(
            dir.path(),
            &CheckpointEntry {
                mailbox: "INBOX".to_string(),
                uid: 1,
                message_hash: "hash1".to_string(),
                md_staged_relpath: "inbox/1.md".to_string(),
                desired_md_name: "1.md".to_string(),
                mailbox_tag: "mailbox/inbox".to_string(),
                attachments: Vec::new(),
            },
        )
        .unwrap();
        append_checkpoint(
            dir.path(),
            &CheckpointEntry {
                mailbox: "Archive".to_string(),
                uid: 2,
                message_hash: "hash2".to_string(),
                md_staged_relpath: "archive/2.md".to_string(),
                desired_md_name: "2.md".to_string(),
                mailbox_tag: "mailbox/archive".to_string(),
                attachments: Vec::new(),
            },
        )
        .unwrap();

        clear_checkpoint_for_mailbox(dir.path(), "INBOX").unwrap();

        let loaded = load_checkpoint(dir.path()).unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].mailbox, "Archive");
    }

    #[test]
    fn load_checkpoint_skips_malformed_lines() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join(CHECKPOINT_FILE_NAME),
            "INBOX\t5\thash\tinbox/5.md\t5.md\tmailbox/inbox\t\nnot-enough-fields\nINBOX\t6\thash2\tinbox/6.md\t6.md\tmailbox/inbox\t\n",
        )
        .unwrap();

        let loaded = load_checkpoint(dir.path()).unwrap();
        assert_eq!(loaded.len(), 2);
        assert_eq!(loaded[0].uid, 5);
        assert_eq!(loaded[1].uid, 6);
    }

    #[test]
    fn done_uids_builds_composite_key_set() {
        let entries = vec![
            CheckpointEntry {
                mailbox: "INBOX".to_string(),
                uid: 5,
                message_hash: "abc".to_string(),
                md_staged_relpath: "inbox/5.md".to_string(),
                desired_md_name: "2024-01-26-hello.md".to_string(),
                mailbox_tag: "mailbox/inbox".to_string(),
                attachments: Vec::new(),
            },
            CheckpointEntry {
                mailbox: "Archive".to_string(),
                uid: 5,
                message_hash: "def".to_string(),
                md_staged_relpath: "archive/5.md".to_string(),
                desired_md_name: "2024-01-26-hello.md".to_string(),
                mailbox_tag: "mailbox/archive".to_string(),
                attachments: Vec::new(),
            },
        ];

        let done = done_uids(&entries);
        assert!(done.contains(&("INBOX".to_string(), 5)));
        assert!(done.contains(&("Archive".to_string(), 5)));
        assert!(!done.contains(&("INBOX".to_string(), 6)));
    }

    #[test]
    fn split_into_batches_empty_input_is_empty() {
        assert!(split_into_batches(&[], 4).is_empty());
    }

    #[test]
    fn split_into_batches_covers_every_uid_with_no_duplicates() {
        let uids: Vec<u32> = (1..=1000).collect();
        let batches = split_into_batches(&uids, 4);

        let mut seen: Vec<u32> = batches.into_iter().flatten().collect();
        seen.sort_unstable();
        assert_eq!(seen, uids);
    }

    #[test]
    fn split_into_batches_count_scales_with_concurrency() {
        let uids: Vec<u32> = (1..=1000).collect();

        let low_concurrency_batches = split_into_batches(&uids, 1).len();
        let high_concurrency_batches = split_into_batches(&uids, 8).len();

        // Batch count comfortably exceeds concurrency at both levels, and
        // more workers means more (smaller) batches for the same input.
        assert!(low_concurrency_batches > 1);
        assert!(high_concurrency_batches > low_concurrency_batches);
    }

    #[test]
    fn split_into_batches_single_uid_is_one_batch() {
        assert_eq!(split_into_batches(&[42], 8), vec![vec![42]]);
    }

    #[test]
    fn split_into_batches_more_batches_than_concurrency_for_a_large_mailbox() {
        // The direct regression case for Context problem 1: a mailbox with
        // thousands of pending messages must yield far more than one batch
        // per mailbox, so a fixed worker pool actually stays busy.
        let uids: Vec<u32> = (1..=5000).collect();
        let concurrency = 4;

        let batch_count = split_into_batches(&uids, concurrency).len();

        assert!(batch_count > concurrency);
    }
}
