//! Content-hash dedup and placement of extracted attachments (ADR-0081
//! §6). Unlike `email_sync::dedup::run_dedup_pass`, there's only ever one
//! pass here, over attachments alone -- no message-level dedup exists for
//! this job (raw `.eml` files are never deduped against each other, and
//! there's no frontmatter to amend even if they were), so this mirrors
//! `pull_transform::dedup::place_files`'s simpler shape instead: a
//! content-hash hit discards the scratch copy, a miss places it.

use std::fs;
use std::path::Path;

use indicatif::MultiProgress;

use crate::commands::job::email_sync::sink;
use crate::core::data::{ContentIndex, Dedup, unique_path};

use super::manifest::PullCheckpointEntry;

pub(crate) const ATTACHMENT_HASHES_FILE: &str = ".attachment-hashes";

pub(crate) struct EmailPullDedup(pub(crate) ContentIndex);

impl Dedup for EmailPullDedup {
    fn check(&self, hash: &str) -> Option<&str> {
        self.0.check(hash)
    }

    fn commit(&mut self, hash: &str, relative_path: &str) -> Result<(), String> {
        self.0.commit(hash, relative_path)
    }
}

/// Outcome of a completed placement pass, for the caller's summary line.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct DedupSummary {
    pub placed: usize,
    pub duplicates_skipped: usize,
}

/// Places every attachment referenced by `entries` under
/// `identity_dir/attachments/`, deduplicating by content hash
/// (`attachment_index`). `entries` is sorted by `(mailbox, uid)` first for
/// reproducible canonical-occurrence selection across re-runs, matching
/// `email_sync::dedup::run_dedup_pass`'s own ordering discipline.
///
/// A scratch path that no longer exists is silently skipped (idempotency
/// guard: already placed or deduped by a prior run of this same pass) --
/// same pattern `email_sync::dedup::run_dedup_pass` and
/// `pull_transform::dedup::place_files` both already rely on.
pub(crate) fn place_attachments(
    identity_dir: &Path,
    staging_dir: &Path,
    entries: &mut [PullCheckpointEntry],
    attachment_index: &mut EmailPullDedup,
    multi_progress: &MultiProgress,
) -> Result<DedupSummary, String> {
    entries.sort_by(|a, b| (&a.mailbox, a.uid).cmp(&(&b.mailbox, b.uid)));

    let total: u64 = entries
        .iter()
        .map(|entry| entry.attachments.len() as u64)
        .sum();
    let bar = sink::new_progress_bar("dedup".to_string(), total, multi_progress);
    let mut summary = DedupSummary::default();

    for entry in entries.iter() {
        for (hash, relpath) in &entry.attachments {
            bar.inc(1);
            let scratch_path = staging_dir.join(relpath);
            if !scratch_path.exists() {
                continue;
            }

            match attachment_index.check(hash) {
                Some(_) => {
                    let _ = fs::remove_file(&scratch_path);
                    summary.duplicates_skipped += 1;
                }
                None => {
                    place_one(identity_dir, &scratch_path, hash, attachment_index)?;
                    summary.placed += 1;
                }
            }
        }
    }

    bar.finish();
    Ok(summary)
}

fn place_one(
    identity_dir: &Path,
    scratch_path: &Path,
    hash: &str,
    attachment_index: &mut EmailPullDedup,
) -> Result<(), String> {
    let attachments_dir = identity_dir.join("attachments");
    fs::create_dir_all(&attachments_dir)
        .map_err(|err| format!("failed to create {}: {err}", attachments_dir.display()))?;

    let file_name = scratch_path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "attachment".to_string());
    let final_path = unique_path(&attachments_dir.join(&file_name));

    fs::rename(scratch_path, &final_path).map_err(|err| {
        format!(
            "failed to move {} to {}: {err}",
            scratch_path.display(),
            final_path.display()
        )
    })?;

    let final_relpath = format!(
        "attachments/{}",
        final_path.file_name().unwrap().to_string_lossy()
    );
    attachment_index.commit(hash, &final_relpath)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dedup_at(dir: &Path) -> EmailPullDedup {
        EmailPullDedup(ContentIndex::load(dir, ATTACHMENT_HASHES_FILE).unwrap())
    }

    fn stage_scratch(staging: &Path, relpath: &str, contents: &[u8]) -> String {
        let path = staging.join(relpath);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, contents).unwrap();
        relpath.to_string()
    }

    fn entry(mailbox: &str, uid: u32, attachments: Vec<(&str, &str)>) -> PullCheckpointEntry {
        PullCheckpointEntry {
            mailbox: mailbox.to_string(),
            uid,
            attachments: attachments
                .into_iter()
                .map(|(hash, relpath)| (hash.to_string(), relpath.to_string()))
                .collect(),
        }
    }

    #[test]
    fn place_attachments_places_a_lone_attachment() {
        let staging = tempfile::tempdir().unwrap();
        let identity_dir = tempfile::tempdir().unwrap();
        let mut dedup = dedup_at(staging.path());

        let relpath = stage_scratch(staging.path(), "inbox/1/attachments/a.pdf", b"content");
        let mut entries = vec![entry("INBOX", 1, vec![("hash-a", &relpath)])];

        let summary = place_attachments(
            identity_dir.path(),
            staging.path(),
            &mut entries,
            &mut dedup,
            &MultiProgress::new(),
        )
        .unwrap();

        assert_eq!(summary.placed, 1);
        assert_eq!(summary.duplicates_skipped, 0);
        assert!(identity_dir.path().join("attachments/a.pdf").exists());
        assert_eq!(dedup.check("hash-a"), Some("attachments/a.pdf"));
    }

    #[test]
    fn place_attachments_dedupes_cross_message_attachment() {
        let staging = tempfile::tempdir().unwrap();
        let identity_dir = tempfile::tempdir().unwrap();
        let mut dedup = dedup_at(staging.path());

        let first = stage_scratch(staging.path(), "inbox/1/attachments/a.pdf", b"same-bytes");
        let second = stage_scratch(staging.path(), "inbox/2/attachments/a.pdf", b"same-bytes");
        let mut entries = vec![
            entry("INBOX", 1, vec![("same-hash", &first)]),
            entry("INBOX", 2, vec![("same-hash", &second)]),
        ];

        let summary = place_attachments(
            identity_dir.path(),
            staging.path(),
            &mut entries,
            &mut dedup,
            &MultiProgress::new(),
        )
        .unwrap();

        assert_eq!(summary.placed, 1);
        assert_eq!(summary.duplicates_skipped, 1);
        let attachments_dir = identity_dir.path().join("attachments");
        assert_eq!(fs::read_dir(&attachments_dir).unwrap().count(), 1);
    }

    #[test]
    fn place_attachments_disambiguates_same_named_attachments() {
        let staging = tempfile::tempdir().unwrap();
        let identity_dir = tempfile::tempdir().unwrap();
        let mut dedup = dedup_at(staging.path());

        let first = stage_scratch(staging.path(), "inbox/1/attachments/a.pdf", b"aaa");
        let second = stage_scratch(staging.path(), "archive/2/attachments/a.pdf", b"bbb");
        let mut entries = vec![
            entry("Archive", 2, vec![("hash-b", &second)]),
            entry("INBOX", 1, vec![("hash-a", &first)]),
        ];

        let summary = place_attachments(
            identity_dir.path(),
            staging.path(),
            &mut entries,
            &mut dedup,
            &MultiProgress::new(),
        )
        .unwrap();

        assert_eq!(summary.placed, 2);
        assert!(identity_dir.path().join("attachments/a.pdf").exists());
        assert!(identity_dir.path().join("attachments/a-2.pdf").exists());
    }

    #[test]
    fn place_attachments_skips_a_missing_scratch_path() {
        let staging = tempfile::tempdir().unwrap();
        let identity_dir = tempfile::tempdir().unwrap();
        let mut dedup = dedup_at(staging.path());

        let mut entries = vec![entry(
            "INBOX",
            1,
            vec![("hash-a", "inbox/1/attachments/missing.pdf")],
        )];

        let summary = place_attachments(
            identity_dir.path(),
            staging.path(),
            &mut entries,
            &mut dedup,
            &MultiProgress::new(),
        )
        .unwrap();

        assert_eq!(summary, DedupSummary::default());
    }
}
