use std::collections::HashSet;
use std::fs;
use std::io::Write;
use std::path::Path;

const CHECKPOINT_FILE_NAME: &str = ".job-checkpoint";

/// One fully fetched `(mailbox, UID)`, recorded in `.job-checkpoint`
/// (ADR-0081). Unlike `email_sync::manifest::CheckpointEntry`, there's no
/// `message_hash`/`md_staged_relpath`/`desired_md_name`/`mailbox_tag` --
/// no message-level dedup or frontmatter exists for this job (ADR-0081 §6
/// scopes dedup to attachments only), and the raw `.eml` is already at its
/// final location by the time this is appended, so nothing about where it
/// ended up needs recording either.
pub(crate) struct PullCheckpointEntry {
    /// Raw IMAP mailbox name -- matches `ManifestEntry::mailbox` for
    /// `done_uids` membership checks (same convention as
    /// `email_sync::manifest::CheckpointEntry`).
    pub mailbox: String,
    pub uid: u32,
    /// `(content-hash, scratch-relpath)` pairs, staging-root-relative --
    /// empty when extraction found zero attachments, including when
    /// `mail_parser` failed to parse the message at all (a parse failure
    /// never drops the checkpoint entry itself; the `.eml` is already
    /// safely on disk regardless of whether it could be parsed).
    pub attachments: Vec<(String, String)>,
}

/// Appends one entry to `staging_dir/.job-checkpoint`. Append-only, one
/// entry at a time, so a crash mid-run never loses already-recorded
/// progress -- same discipline as `email_sync::manifest::append_checkpoint`.
/// Format: `<mailbox>\t<uid>\t<hash>=<relpath>[;<hash>=<relpath>...]` (the
/// last field is empty for a message with no attachments).
pub(crate) fn append_checkpoint(
    staging_dir: &Path,
    entry: &PullCheckpointEntry,
) -> Result<(), String> {
    let path = staging_dir.join(CHECKPOINT_FILE_NAME);
    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .map_err(|err| format!("failed to open {}: {err}", path.display()))?;
    writeln!(file, "{}", format_checkpoint_line(entry))
        .map_err(|err| format!("failed to write {}: {err}", path.display()))
}

fn format_checkpoint_line(entry: &PullCheckpointEntry) -> String {
    let attachments = entry
        .attachments
        .iter()
        .map(|(hash, relpath)| format!("{hash}={relpath}"))
        .collect::<Vec<_>>()
        .join(";");
    format!("{}\t{}\t{}", entry.mailbox, entry.uid, attachments)
}

/// Loads every entry recorded in `staging_dir/.job-checkpoint`. A missing
/// file (first run) is an empty list. Malformed lines are skipped
/// leniently, matching every other loader in this codebase.
pub(crate) fn load_checkpoint(staging_dir: &Path) -> Result<Vec<PullCheckpointEntry>, String> {
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
            Some(PullCheckpointEntry {
                mailbox,
                uid,
                attachments,
            })
        })
        .collect())
}

/// Rewrites `.job-checkpoint`, dropping every entry for `mailbox` -- used
/// when that mailbox's `UIDVALIDITY` no longer matches the server's (a
/// recreated mailbox), so stale UID/message pairings don't linger forever
/// as false "already done" markers. Mirrors
/// `email_sync::manifest::clear_checkpoint_for_mailbox`.
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

/// The set of `(mailbox, UID)` pairs already recorded as done -- an O(1)
/// membership check for deciding what's still pending. Mirrors
/// `email_sync::manifest::done_uids`.
pub(crate) fn done_uids(entries: &[PullCheckpointEntry]) -> HashSet<(String, u32)> {
    entries
        .iter()
        .map(|entry| (entry.mailbox.clone(), entry.uid))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

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
            &PullCheckpointEntry {
                mailbox: "INBOX".to_string(),
                uid: 5,
                attachments: Vec::new(),
            },
        )
        .unwrap();
        append_checkpoint(
            dir.path(),
            &PullCheckpointEntry {
                mailbox: "INBOX".to_string(),
                uid: 9,
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
            &PullCheckpointEntry {
                mailbox: "INBOX".to_string(),
                uid: 1,
                attachments: Vec::new(),
            },
        )
        .unwrap();
        append_checkpoint(
            dir.path(),
            &PullCheckpointEntry {
                mailbox: "Archive".to_string(),
                uid: 2,
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
            "INBOX\t5\t\nnot-enough-fields\nINBOX\t6\t\n",
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
            PullCheckpointEntry {
                mailbox: "INBOX".to_string(),
                uid: 5,
                attachments: Vec::new(),
            },
            PullCheckpointEntry {
                mailbox: "Archive".to_string(),
                uid: 5,
                attachments: Vec::new(),
            },
        ];

        let done = done_uids(&entries);
        assert!(done.contains(&("INBOX".to_string(), 5)));
        assert!(done.contains(&("Archive".to_string(), 5)));
        assert!(!done.contains(&("INBOX".to_string(), 6)));
    }
}
