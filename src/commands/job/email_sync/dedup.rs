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
/// cover them) and are simply deleted alongside its staged `.md`. The
/// attachment pass resolves each entry's canonical `.md` path via
/// `message_index` rather than tracking what the message pass placed *in
/// this call* -- `message_index` persists across runs, so a message
/// canonicalized by an earlier call is just as eligible for attachment
/// placement as one canonicalized moments ago (ADR-0030 amendment: this is
/// what actually makes a re-run recover previously-orphaned attachments).
pub(crate) fn run_dedup_pass(
    identity_dir: &Path,
    staging_dir: &Path,
    entries: &mut [CheckpointEntry],
    message_index: &mut EmailDedup,
    attachment_index: &mut EmailDedup,
) -> Result<DedupSummary, String> {
    entries.sort_by(|a, b| (&a.mailbox, a.uid).cmp(&(&b.mailbox, b.uid)));

    let mut summary = DedupSummary::default();

    for entry in entries.iter() {
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
                        place_canonical_message(identity_dir, staging_dir, entry, message_index)?;
                    }
                }
            }
            None => {
                place_canonical_message(identity_dir, staging_dir, entry, message_index)?;
            }
        }
    }

    for entry in entries.iter() {
        // Resolves this entry's canonical destination via `message_index`
        // rather than trusting that this same call is what placed it --
        // `message_index` is loaded from its persisted file at the top of
        // every call, so a hash committed by an *earlier* run is just as
        // visible here as one committed moments ago in the loop above. A
        // duplicate entry's own staged attachments are already deleted by
        // `remove_staged_files` whenever it was merged (this run or an
        // earlier one), so the per-attachment `exists()` check below still
        // correctly no-ops for them (ADR-0030 amendment).
        let Some(message_canonical_relpath) = message_index.check(&entry.message_hash) else {
            continue;
        };
        let md_path = identity_dir.join(message_canonical_relpath);
        for (hash, staged_relpath) in &entry.attachments {
            let staged_path = staged_attachment_path(staging_dir, entry, staged_relpath);
            if !staged_path.exists() {
                // Same idempotency guard as the message-level pass above.
                continue;
            }
            match attachment_index.check(hash) {
                Some(canonical_relpath) => {
                    summary.deduped_attachments += 1;
                    let _ = fs::remove_file(&staged_path);
                    data::rewrite_attachment_reference(
                        &md_path,
                        staged_relpath,
                        canonical_relpath,
                    )?;
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
                            &md_path,
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
/// via `unique_path`, and commits its hash to `message_index`. The
/// attachment-placement pass above re-derives this same final path itself
/// (via `message_index.check`) rather than being handed it directly, so it
/// works the same way whether this entry's message was placed in this call
/// or an earlier one (ADR-0030 amendment).
fn place_canonical_message(
    identity_dir: &Path,
    staging_dir: &Path,
    entry: &CheckpointEntry,
    message_index: &mut EmailDedup,
) -> Result<(), String> {
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
    Ok(())
}

/// Deletes a merged duplicate's staged `.md` and every staged attachment it
/// referenced -- all redundant once the entry is merged into an existing
/// canonical file.
fn remove_staged_files(staging_dir: &Path, entry: &CheckpointEntry) {
    let _ = fs::remove_file(staging_dir.join(&entry.md_staged_relpath));
    for (_, relpath) in &entry.attachments {
        let _ = fs::remove_file(staged_attachment_path(staging_dir, entry, relpath));
    }
}

/// The real, currently-staged location of an attachment. `relpath`
/// (`entry.attachments`'s second element) is deliberately *not*
/// staging-root-relative like `md_staged_relpath` is -- it's the
/// attachment's *final*, `identity_dir`-relative frontmatter path
/// (`attachments/<name>`), reused unchanged once placed. The staged copy
/// actually lives nested under the message's own UID-keyed staging
/// directory (`transform.rs`'s `attachments_dir`), reconstructed here from
/// fields already on `entry`: `md_staged_relpath`'s parent directory
/// (`transformed/<mailbox_relpath>`) plus `entry.uid` (ADR-0030).
fn staged_attachment_path(staging_dir: &Path, entry: &CheckpointEntry, relpath: &str) -> PathBuf {
    let md_parent = Path::new(&entry.md_staged_relpath)
        .parent()
        .unwrap_or_else(|| Path::new(""));
    let file_name = Path::new(relpath)
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| relpath.to_string());
    staging_dir
        .join(md_parent)
        .join(entry.uid.to_string())
        .join("attachments")
        .join(file_name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::job::email_sync::{manifest, transform};

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
        // Real `EmailTransform` layout: staged under each message's own
        // UID-keyed directory, not directly under `staging_dir` (ADR-0030)
        // -- different staged paths (per-uid staging tree keeps them apart)
        // but identical content hash.
        stage_attachment(
            staging.path(),
            "transformed/Archive/1/attachments/a.pdf",
            b"content-1",
        );
        stage_attachment(
            staging.path(),
            "transformed/INBOX/2/attachments/a.pdf",
            b"content-1",
        );

        let mut entries = vec![first, second];

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

    /// Regression coverage for the ADR-0030 amendment (Finding 2): a
    /// message that was already canonicalized by an *earlier* run (its
    /// staged `.md` is gone and its hash is already committed to
    /// `message_index`, exactly what a prior `run_dedup_pass` call would
    /// have left behind) must still have its attachment placed if that
    /// attachment is still sitting, unplaced, in staging -- this is what
    /// makes a re-run actually self-healing, rather than only working when
    /// message and attachment happen to be placed in the very same call.
    #[test]
    fn run_dedup_pass_places_attachment_for_a_message_already_canonicalized_by_an_earlier_run() {
        let staging = tempfile::tempdir().unwrap();
        let identity_dir = tempfile::tempdir().unwrap();
        let (mut message_index, mut attachment_index) = indexes(staging.path());

        let e = entry(
            "INBOX",
            1,
            "hash-a",
            "2024-01-26-hello.md",
            vec![("attach-hash", "attachments/a.pdf")],
        );

        // Simulate the state left behind by an earlier `run_dedup_pass`
        // call: the message is already at its final location and its hash
        // already committed, but its staged `.md` is gone (so this call's
        // message-placement pass has nothing to do for it) while its
        // attachment is still sitting, untouched, in staging.
        fs::write(
            identity_dir.path().join("2024-01-26-hello.md"),
            FIXTURE_BODY,
        )
        .unwrap();
        message_index
            .commit("hash-a", "2024-01-26-hello.md")
            .unwrap();
        stage_attachment(
            staging.path(),
            "transformed/INBOX/1/attachments/a.pdf",
            b"content",
        );

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
        let attachments_dir = identity_dir.path().join("attachments");
        assert_eq!(
            fs::read_dir(&attachments_dir)
                .unwrap_or_else(|err| panic!("{} should exist: {err}", attachments_dir.display()))
                .count(),
            1,
            "the attachment should be placed even though its message was canonicalized before this call"
        );
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

    /// Regression coverage for ADR-0030: exercises the real pipeline the
    /// other tests in this module don't -- real `EmailTransform::transform`
    /// staging an attachment, a real `append_checkpoint`/`load_checkpoint`
    /// round-trip (not a hand-built `CheckpointEntry`), then real
    /// `run_dedup_pass` -- the exact seam where the staged-path
    /// reconstruction bug lived undetected.
    #[test]
    fn run_dedup_pass_places_attachment_from_real_transform_and_checkpoint_round_trip() {
        use crate::commands::keyring::email::identity::Identity;
        use crate::commands::keyring::email::provider::Provider;
        use crate::core::data::Transform;
        use transform::EmailTransform;

        let staging = tempfile::tempdir().unwrap();
        let identity_dir = tempfile::tempdir().unwrap();
        let input = tempfile::tempdir().unwrap();
        let inbox = input.path().join("inbox");
        fs::create_dir_all(&inbox).unwrap();

        let eml = "From: Jane Doe <jane.doe@example.com>\r\n\
            To: first.last@example.com\r\n\
            Subject: Shipping\r\n\
            Date: Fri, 26 Jan 2024 09:15:00 +0000\r\n\
            MIME-Version: 1.0\r\n\
            Content-Type: multipart/mixed; boundary=\"BOUNDARY\"\r\n\
            \r\n\
            --BOUNDARY\r\n\
            Content-Type: text/plain; charset=utf-8\r\n\
            \r\n\
            Hello there!\r\n\
            --BOUNDARY\r\n\
            Content-Type: application/pdf\r\n\
            Content-Disposition: attachment; filename=\"a.pdf\"\r\n\
            Content-Transfer-Encoding: base64\r\n\
            \r\n\
            JVBERi0xLjQK\r\n\
            --BOUNDARY--\r\n";
        fs::write(inbox.join("1.eml"), eml).unwrap();

        let transformer = EmailTransform {
            identity: Identity {
                alias: "first-last".to_string(),
                email: "first.last@example.com".to_string(),
                provider: Provider::Gmail,
                host: "imap.gmail.com".to_string(),
                port: 993,
            },
            input_root: input.path().to_path_buf(),
            staging_root: staging.path().to_path_buf(),
        };
        let outcome = transformer.transform(inbox.join("1.eml")).unwrap().unwrap();
        assert_eq!(
            outcome.attachments.len(),
            1,
            "fixture message should have exactly one attachment"
        );

        let checkpoint_entry = CheckpointEntry {
            mailbox: "inbox".to_string(),
            uid: 1,
            message_hash: outcome.message_hash,
            md_staged_relpath: outcome.md_staged_relpath,
            desired_md_name: outcome.desired_md_name,
            mailbox_tag: outcome.mailbox_tag,
            attachments: outcome
                .attachments
                .into_iter()
                .map(|attachment| (attachment.hash, attachment.staged_relpath))
                .collect(),
        };
        manifest::append_checkpoint(staging.path(), &checkpoint_entry).unwrap();

        let mut entries = manifest::load_checkpoint(staging.path()).unwrap();
        let (mut message_index, mut attachment_index) = indexes(staging.path());

        run_dedup_pass(
            identity_dir.path(),
            staging.path(),
            &mut entries,
            &mut message_index,
            &mut attachment_index,
        )
        .unwrap();

        let attachments_dir = identity_dir.path().join("attachments");
        let placed = fs::read_dir(&attachments_dir)
            .unwrap_or_else(|err| panic!("{} should exist: {err}", attachments_dir.display()))
            .count();
        assert_eq!(
            placed, 1,
            "the real staged attachment should be placed under identity_dir/attachments/"
        );
    }
}
