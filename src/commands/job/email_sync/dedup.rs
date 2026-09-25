use std::fs;
use std::path::{Path, PathBuf};

use crate::core::data::{self, ContentIndex, Dedup};

use super::manifest::CheckpointEntry;

/// The email-specific `Dedup` implementor (ADR-0023): wraps a generic
/// `ContentIndex` (ADR-0020) and forwards straight through -- nothing about
/// *how* a hash maps to a path is email-specific; only `run_dedup_pass`'s
/// use of a hit/miss (merge a duplicate message vs. reuse a duplicate
/// attachment) is.
pub(crate) struct EmailDedup(pub(crate) ContentIndex);

impl Dedup for EmailDedup {
    fn check(&self, hash: &str) -> Option<&str> {
        self.0.check(hash)
    }

    fn commit(&mut self, hash: &str, relative_path: &str) -> Result<(), String> {
        self.0.commit(hash, relative_path)
    }
}

/// Outcome of a completed dedup pass, for the caller's summary line.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct DedupSummary {
    pub merged_messages: usize,
    pub deduped_attachments: usize,
}

/// The single-threaded, post-transform dedup pass (ADR-0021 §7/§10): places
/// every checkpointed message and attachment at its final, canonical
/// location under `identity_dir` (the flat, identity-rooted tree, ADR-0006),
/// merging real content-hash duplicates instead of writing them twice, and
/// closing the `unique_path` TOCTOU race (ADR-0021 addendum) by being the
/// only caller of `unique_path` against that shared tree.
///
/// `entries` is sorted by `(mailbox, uid)` ascending first -- batches
/// complete out of order under concurrency, so checkpoint append order isn't
/// deterministic across runs, and reproducible canonical-occurrence
/// selection requires a stable processing order.
///
/// Runs in two passes over the sorted entries: messages first (so every
/// canonical message's final path is settled), then attachments of
/// canonical (non-merged) messages only -- a merged duplicate's own
/// attachments are redundant by construction (identical message hash means
/// identical raw bytes, so the canonical message's own attachments already
/// cover them) and are simply deleted alongside its staged `.md`.
pub(crate) fn run_dedup_pass(
    identity_dir: &Path,
    staging_dir: &Path,
    entries: &mut [CheckpointEntry],
    message_index: &mut EmailDedup,
    attachment_index: &mut EmailDedup,
) -> Result<DedupSummary, String> {
    entries.sort_by(|a, b| (&a.mailbox, a.uid).cmp(&(&b.mailbox, b.uid)));

    let mut summary = DedupSummary::default();
    let mut placed_md_paths = vec![None; entries.len()];

    for (index, entry) in entries.iter().enumerate() {
        if !staging_dir.join(&entry.md_staged_relpath).exists() {
            // Already fully handled by a prior dedup pass run (placed as
            // canonical, or merged as a duplicate -- either way its staged
            // `.md` was moved or deleted). `message_index` only records the
            // current canonical path per hash, not which specific entry
            // produced it, so re-deriving "was this entry canonical or a
            // duplicate" from the index alone isn't reliable -- checking
            // `message_index.check()` again here could wrongly treat an
            // already-canonical entry as a self-duplicate of itself.
            // Skipping whenever the staged file is already gone sidesteps
            // that ambiguity entirely and is always safe: there is nothing
            // left for this entry to do.
            continue;
        }

        match message_index.check(&entry.message_hash) {
            Some(canonical_relpath) => {
                let canonical_path = identity_dir.join(canonical_relpath);
                match data::amend_frontmatter_for_duplicate(
                    &canonical_path,
                    &entry.mailbox_tag,
                    entry.uid,
                ) {
                    Ok(_) => {
                        summary.merged_messages += 1;
                        remove_staged_files(staging_dir, entry);
                    }
                    Err(err) => {
                        eprintln!(
                            "Warning: canonical file for duplicate {} is missing or malformed: {err}, treating as canonical instead",
                            entry.md_staged_relpath
                        );
                        placed_md_paths[index] = Some(place_canonical_message(
                            identity_dir,
                            staging_dir,
                            entry,
                            message_index,
                        )?);
                    }
                }
            }
            None => {
                placed_md_paths[index] = Some(place_canonical_message(
                    identity_dir,
                    staging_dir,
                    entry,
                    message_index,
                )?);
            }
        }
    }

    for (index, entry) in entries.iter().enumerate() {
        let Some(md_path) = &placed_md_paths[index] else {
            continue;
        };
        for (hash, staged_relpath) in &entry.attachments {
            let staged_path = staging_dir.join(staged_relpath);
            if !staged_path.exists() {
                // Same idempotency guard as the message-level pass above.
                continue;
            }
            match attachment_index.check(hash) {
                Some(canonical_relpath) => {
                    summary.deduped_attachments += 1;
                    let _ = fs::remove_file(&staged_path);
                    data::rewrite_attachment_reference(md_path, staged_relpath, canonical_relpath)?;
                }
                None => {
                    let file_name = Path::new(staged_relpath)
                        .file_name()
                        .map(|name| name.to_string_lossy().into_owned())
                        .unwrap_or_else(|| staged_relpath.clone());
                    let attachments_dir = identity_dir.join("attachments");
                    fs::create_dir_all(&attachments_dir).map_err(|err| {
                        format!("failed to create {}: {err}", attachments_dir.display())
                    })?;
                    let final_path = data::unique_path(&attachments_dir.join(&file_name));
                    fs::rename(&staged_path, &final_path).map_err(|err| {
                        format!(
                            "failed to move {} to {}: {err}",
                            staged_path.display(),
                            final_path.display()
                        )
                    })?;
                    let final_relpath = format!(
                        "attachments/{}",
                        final_path.file_name().unwrap().to_string_lossy()
                    );
                    attachment_index.commit(hash, &final_relpath)?;
                    if final_relpath != *staged_relpath {
                        data::rewrite_attachment_reference(
                            md_path,
                            staged_relpath,
                            &final_relpath,
                        )?;
                    }
                }
            }
        }
    }

    Ok(summary)
}

/// Places a canonical (non-duplicate) message's staged `.md` at its final
/// location under `identity_dir`, resolving any genuine filename collision
/// via `unique_path`, and commits its hash to `message_index`. Returns the
/// final path, needed by the attachment-placement pass above to target
/// `rewrite_attachment_reference` calls at the right file.
fn place_canonical_message(
    identity_dir: &Path,
    staging_dir: &Path,
    entry: &CheckpointEntry,
    message_index: &mut EmailDedup,
) -> Result<PathBuf, String> {
    fs::create_dir_all(identity_dir)
        .map_err(|err| format!("failed to create {}: {err}", identity_dir.display()))?;
    let final_path = data::unique_path(&identity_dir.join(&entry.desired_md_name));
    let staged_path = staging_dir.join(&entry.md_staged_relpath);
    fs::rename(&staged_path, &final_path).map_err(|err| {
        format!(
            "failed to move {} to {}: {err}",
            staged_path.display(),
            final_path.display()
        )
    })?;
    let final_relpath = final_path
        .file_name()
        .unwrap()
        .to_string_lossy()
        .into_owned();
    message_index.commit(&entry.message_hash, &final_relpath)?;
    Ok(final_path)
}

/// Deletes a merged duplicate's staged `.md` and every staged attachment it
/// referenced -- all redundant once the entry is merged into an existing
/// canonical file.
fn remove_staged_files(staging_dir: &Path, entry: &CheckpointEntry) {
    let _ = fs::remove_file(staging_dir.join(&entry.md_staged_relpath));
    for (_, relpath) in &entry.attachments {
        let _ = fs::remove_file(staging_dir.join(relpath));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::job::email_sync::transform;

    fn entry(
        mailbox: &str,
        uid: u32,
        message_hash: &str,
        desired_md_name: &str,
        attachments: Vec<(&str, &str)>,
    ) -> CheckpointEntry {
        CheckpointEntry {
            mailbox: mailbox.to_string(),
            uid,
            message_hash: message_hash.to_string(),
            md_staged_relpath: format!("transformed/{mailbox}/{uid}.md"),
            desired_md_name: desired_md_name.to_string(),
            mailbox_tag: format!("mailbox/{}", mailbox.to_lowercase()),
            attachments: attachments
                .into_iter()
                .map(|(hash, relpath)| (hash.to_string(), relpath.to_string()))
                .collect(),
        }
    }

    fn stage_message(staging_dir: &Path, entry: &CheckpointEntry, body: &str) {
        let path = staging_dir.join(&entry.md_staged_relpath);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, body).unwrap();
    }

    fn stage_attachment(staging_dir: &Path, relpath: &str, contents: &[u8]) {
        let path = staging_dir.join(relpath);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, contents).unwrap();
    }

    const FIXTURE_BODY: &str = "---\nfrom: \"a\"\ntags:\n  - mailbox/inbox\n---\nbody";

    fn indexes(staging: &Path) -> (EmailDedup, EmailDedup) {
        (
            EmailDedup(ContentIndex::load(staging, transform::MESSAGE_HASHES_FILE).unwrap()),
            EmailDedup(ContentIndex::load(staging, transform::ATTACHMENT_HASHES_FILE).unwrap()),
        )
    }

    #[test]
    fn run_dedup_pass_places_a_lone_canonical_message() {
        let staging = tempfile::tempdir().unwrap();
        let identity_dir = tempfile::tempdir().unwrap();
        let (mut message_index, mut attachment_index) = indexes(staging.path());

        let e = entry("INBOX", 1, "hash-a", "2024-01-26-hello.md", vec![]);
        stage_message(staging.path(), &e, FIXTURE_BODY);
        let mut entries = vec![e];

        let summary = run_dedup_pass(
            identity_dir.path(),
            staging.path(),
            &mut entries,
            &mut message_index,
            &mut attachment_index,
        )
        .unwrap();

        assert_eq!(summary.merged_messages, 0);
        assert!(identity_dir.path().join("2024-01-26-hello.md").exists());
        assert!(!staging.path().join("transformed/INBOX/1.md").exists());
        assert_eq!(message_index.check("hash-a"), Some("2024-01-26-hello.md"));
    }

    #[test]
    fn run_dedup_pass_merges_two_messages_with_the_same_hash() {
        let staging = tempfile::tempdir().unwrap();
        let identity_dir = tempfile::tempdir().unwrap();
        let (mut message_index, mut attachment_index) = indexes(staging.path());

        let first = entry("Archive", 2, "same-hash", "2024-01-26-hello.md", vec![]);
        let second = entry("INBOX", 1, "same-hash", "2024-01-26-hello.md", vec![]);
        stage_message(staging.path(), &first, FIXTURE_BODY);
        stage_message(staging.path(), &second, FIXTURE_BODY);
        let mut entries = vec![second, first];

        let summary = run_dedup_pass(
            identity_dir.path(),
            staging.path(),
            &mut entries,
            &mut message_index,
            &mut attachment_index,
        )
        .unwrap();

        // Sorted order is (Archive, 2) then (INBOX, 1) -- Archive comes
        // first alphabetically, so it becomes canonical.
        assert_eq!(summary.merged_messages, 1);
        let md_files: Vec<_> = fs::read_dir(identity_dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.path().extension().and_then(|x| x.to_str()) == Some("md"))
            .collect();
        assert_eq!(md_files.len(), 1);
        let contents = fs::read_to_string(md_files[0].path()).unwrap();
        assert!(contents.contains("also-in:"));
        assert!(contents.contains("mailbox/inbox#1"));
    }

    #[test]
    fn run_dedup_pass_dedupes_cross_message_attachment_and_rewrites_reference() {
        let staging = tempfile::tempdir().unwrap();
        let identity_dir = tempfile::tempdir().unwrap();
        let (mut message_index, mut attachment_index) = indexes(staging.path());

        let first = entry(
            "Archive",
            1,
            "hash-1",
            "2024-01-26-first.md",
            vec![("attach-hash", "attachments/a.pdf")],
        );
        let second = entry(
            "INBOX",
            2,
            "hash-2",
            "2024-01-27-second.md",
            vec![("attach-hash", "attachments/a.pdf")],
        );
        let body_with_attachment = "---\nfrom: \"a\"\ntags:\n  - mailbox/inbox\nattachments:\n  - attachments/a.pdf\n---\nbody";
        stage_message(staging.path(), &first, body_with_attachment);
        stage_message(staging.path(), &second, body_with_attachment);
        stage_attachment(staging.path(), "attachments/a.pdf", b"content-1");
        // Different staged path (per-uid staging tree keeps them apart) but
        // identical content hash.
        let second_attachment_relpath = "transformed/INBOX/2/attachments/a.pdf";
        stage_attachment(staging.path(), second_attachment_relpath, b"content-1");
        let mut second_with_real_path = second;
        second_with_real_path.attachments = vec![(
            "attach-hash".to_string(),
            second_attachment_relpath.to_string(),
        )];

        let mut entries = vec![first, second_with_real_path];

        let summary = run_dedup_pass(
            identity_dir.path(),
            staging.path(),
            &mut entries,
            &mut message_index,
            &mut attachment_index,
        )
        .unwrap();

        assert_eq!(summary.deduped_attachments, 1);
        let attachments_dir = identity_dir.path().join("attachments");
        assert_eq!(fs::read_dir(&attachments_dir).unwrap().count(), 1);

        let second_md = identity_dir.path().join("2024-01-27-second.md");
        let contents = fs::read_to_string(&second_md).unwrap();
        assert!(contents.contains("attachments:\n  - attachments/a.pdf"));
    }

    #[test]
    fn run_dedup_pass_is_idempotent_when_rerun_on_the_same_entries() {
        // A real caller (the job orchestrator) may pass the same identity's
        // full checkpoint history to every job run, not just newly-added
        // entries -- so a second call with entries already fully placed by
        // a prior call must be a safe no-op, not corrupt the already-
        // canonical file (e.g. by treating it as a duplicate of itself,
        // since `message_index.check()` alone can't distinguish "this entry
        // is the canonical one" from "this is a fresh duplicate" once the
        // hash is committed).
        let staging = tempfile::tempdir().unwrap();
        let identity_dir = tempfile::tempdir().unwrap();
        let (mut message_index, mut attachment_index) = indexes(staging.path());

        let e = entry("INBOX", 1, "hash-a", "2024-01-26-hello.md", vec![]);
        stage_message(staging.path(), &e, FIXTURE_BODY);
        let mut entries = vec![e];

        run_dedup_pass(
            identity_dir.path(),
            staging.path(),
            &mut entries,
            &mut message_index,
            &mut attachment_index,
        )
        .unwrap();
        let after_first_run =
            fs::read_to_string(identity_dir.path().join("2024-01-26-hello.md")).unwrap();

        let summary = run_dedup_pass(
            identity_dir.path(),
            staging.path(),
            &mut entries,
            &mut message_index,
            &mut attachment_index,
        )
        .unwrap();

        assert_eq!(summary, DedupSummary::default());
        assert_eq!(
            fs::read_to_string(identity_dir.path().join("2024-01-26-hello.md")).unwrap(),
            after_first_run,
            "re-running the pass must not append a spurious self-referential also-in entry"
        );
    }
}
