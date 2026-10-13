//! The `.pushed` delivery checkpoint and this job's batching (ADR-0146 §6).
//!
//! Two tab-separated fields, against `email-pull`'s three and
//! `email-sync`'s seven: there are no content hashes to record, because
//! there is no dedup, and no staged-path bookkeeping, because there is no
//! placement step. A row means *delivered*, not *fetched* -- which is what
//! makes `--upload-only` meaningless for this job (ADR-0146 §1, §5).

use std::collections::HashSet;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

/// Append-only, one row per fully delivered UID.
pub(crate) const CHECKPOINT_FILE: &str = ".pushed";

/// One mailbox's worth of work for one worker.
pub(crate) struct Batch {
    pub mailbox: String,
    pub mailbox_relpath: PathBuf,
    pub uids: Vec<u32>,
}

/// One delivered `(mailbox, uid)`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CheckpointEntry {
    pub mailbox: String,
    pub uid: u32,
}

/// Splits `uids` into chunks of exactly `batch_size` (the last chunk may be
/// shorter).
///
/// Deliberately *not* `email_sync::manifest::split_into_batches`, which
/// derives its chunk size from `--concurrency` so that at least
/// `MIN_BATCHES_PER_WORKER` batches exist per worker. This job has no
/// `--concurrency` (ADR-0146 §1) and does have a `--batch-size`, because
/// here the chunk size governs how many messages one `UID FETCH` asks a
/// rate-limited server for at once -- a property of politeness toward the
/// server, not of keeping workers busy.
pub(crate) fn split_into_batches(uids: &[u32], batch_size: usize) -> Vec<Vec<u32>> {
    if uids.is_empty() {
        return Vec::new();
    }
    uids.chunks(batch_size.max(1))
        .map(<[u32]>::to_vec)
        .collect()
}

fn checkpoint_path(staging_dir: &Path) -> PathBuf {
    staging_dir.join(CHECKPOINT_FILE)
}

/// Appends one delivered UID. Called only once every object belonging to
/// that UID is confirmed pushed (ADR-0146 §5 step 6).
pub(crate) fn append_checkpoint(staging_dir: &Path, entry: &CheckpointEntry) -> Result<(), String> {
    fs::create_dir_all(staging_dir)
        .map_err(|err| format!("failed to create {}: {err}", staging_dir.display()))?;
    let path = checkpoint_path(staging_dir);
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .map_err(|err| format!("failed to open {}: {err}", path.display()))?;
    writeln!(file, "{}\t{}", entry.mailbox, entry.uid)
        .map_err(|err| format!("failed to write {}: {err}", path.display()))
}

/// Reads every checkpointed row. A missing file is an empty set (a first
/// run), and a malformed row is skipped leniently rather than failing the
/// run -- both matching the sibling jobs' loaders.
pub(crate) fn load_checkpoint(staging_dir: &Path) -> Result<Vec<CheckpointEntry>, String> {
    let path = checkpoint_path(staging_dir);
    let contents = match fs::read_to_string(&path) {
        Ok(contents) => contents,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => return Err(format!("failed to read {}: {err}", path.display())),
    };

    let mut entries = Vec::new();
    for line in contents.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let Some((mailbox, uid)) = line.split_once('\t') else {
            continue;
        };
        let Ok(uid) = uid.trim().parse::<u32>() else {
            continue;
        };
        entries.push(CheckpointEntry {
            mailbox: mailbox.to_string(),
            uid,
        });
    }
    Ok(entries)
}

/// The composite resume key, differenced against `uid_search("ALL")` via
/// `sink::missing_uids`. Keyed on the **raw** mailbox name rather than its
/// sanitized path, so two mailboxes sanitizing to the same directory still
/// resume independently.
pub(crate) fn done_uids(entries: &[CheckpointEntry]) -> HashSet<(String, u32)> {
    entries
        .iter()
        .map(|entry| (entry.mailbox.clone(), entry.uid))
        .collect()
}

/// Drops one mailbox's rows, rewriting the file. Called on a `UIDVALIDITY`
/// change, where every UID that mailbox ever reported is now meaningless.
pub(crate) fn clear_checkpoint_for_mailbox(
    staging_dir: &Path,
    mailbox: &str,
) -> Result<(), String> {
    let entries = load_checkpoint(staging_dir)?;
    let kept: Vec<&CheckpointEntry> = entries
        .iter()
        .filter(|entry| entry.mailbox != mailbox)
        .collect();
    if kept.len() == entries.len() {
        return Ok(());
    }

    let path = checkpoint_path(staging_dir);
    let body: String = kept
        .iter()
        .map(|entry| format!("{}\t{}\n", entry.mailbox, entry.uid))
        .collect();
    fs::write(&path, body).map_err(|err| format!("failed to write {}: {err}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(mailbox: &str, uid: u32) -> CheckpointEntry {
        CheckpointEntry {
            mailbox: mailbox.to_string(),
            uid,
        }
    }

    #[test]
    fn split_into_batches_honors_an_explicit_batch_size() {
        // The distinguishing behavior against email-sync's
        // concurrency-derived chunker: the size asked for is the size used.
        let uids: Vec<u32> = (1..=50).collect();

        let batches = split_into_batches(&uids, 20);

        assert_eq!(batches.len(), 3);
        assert_eq!(batches[0].len(), 20);
        assert_eq!(batches[1].len(), 20);
        assert_eq!(batches[2].len(), 10);
        assert_eq!(batches.concat(), uids, "every uid appears exactly once");
    }

    #[test]
    fn split_into_batches_handles_an_empty_input_and_a_zero_size() {
        assert!(split_into_batches(&[], 20).is_empty());
        // A zero would panic `chunks`; clamped to 1 instead.
        assert_eq!(split_into_batches(&[1, 2], 0).len(), 2);
    }

    #[test]
    fn load_checkpoint_treats_a_missing_file_as_empty() {
        let dir = tempfile::tempdir().unwrap();
        assert!(load_checkpoint(dir.path()).unwrap().is_empty());
    }

    #[test]
    fn checkpoint_round_trips_across_separate_appends() {
        let dir = tempfile::tempdir().unwrap();

        append_checkpoint(dir.path(), &entry("INBOX", 1)).unwrap();
        append_checkpoint(dir.path(), &entry("Archive/2020", 7)).unwrap();

        let entries = load_checkpoint(dir.path()).unwrap();
        assert_eq!(entries, vec![entry("INBOX", 1), entry("Archive/2020", 7)]);
    }

    #[test]
    fn append_checkpoint_creates_a_staging_dir_that_does_not_exist_yet() {
        let dir = tempfile::tempdir().unwrap();
        let staging = dir.path().join("nested").join(".staging");

        append_checkpoint(&staging, &entry("INBOX", 1)).unwrap();

        assert_eq!(load_checkpoint(&staging).unwrap().len(), 1);
    }

    #[test]
    fn load_checkpoint_skips_malformed_lines_leniently() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join(CHECKPOINT_FILE),
            "INBOX\t1\nno-tab-here\nINBOX\tnot-a-number\n\nINBOX\t2\n",
        )
        .unwrap();

        let entries = load_checkpoint(dir.path()).unwrap();

        assert_eq!(entries, vec![entry("INBOX", 1), entry("INBOX", 2)]);
    }

    #[test]
    fn done_uids_keys_on_the_mailbox_and_uid_together() {
        let entries = vec![entry("INBOX", 1), entry("Archive", 1)];

        let done = done_uids(&entries);

        assert_eq!(done.len(), 2, "the same uid in two mailboxes is two keys");
        assert!(done.contains(&("INBOX".to_string(), 1)));
        assert!(done.contains(&("Archive".to_string(), 1)));
    }

    #[test]
    fn clear_checkpoint_for_mailbox_removes_only_that_mailboxs_rows() {
        let dir = tempfile::tempdir().unwrap();
        append_checkpoint(dir.path(), &entry("INBOX", 1)).unwrap();
        append_checkpoint(dir.path(), &entry("Archive", 2)).unwrap();
        append_checkpoint(dir.path(), &entry("INBOX", 3)).unwrap();

        clear_checkpoint_for_mailbox(dir.path(), "INBOX").unwrap();

        assert_eq!(
            load_checkpoint(dir.path()).unwrap(),
            vec![entry("Archive", 2)]
        );
    }

    #[test]
    fn clear_checkpoint_for_mailbox_is_a_no_op_for_an_unknown_mailbox() {
        let dir = tempfile::tempdir().unwrap();
        append_checkpoint(dir.path(), &entry("INBOX", 1)).unwrap();

        clear_checkpoint_for_mailbox(dir.path(), "Nope").unwrap();

        assert_eq!(
            load_checkpoint(dir.path()).unwrap(),
            vec![entry("INBOX", 1)]
        );
    }
}
