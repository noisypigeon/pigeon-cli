use std::fs;
use std::path::{Path, PathBuf};

use mail_parser::{Addr, DateTime, MessageParser, MimeHeaders};

use crate::commands::keyring::email::identity::{self, Identity};
use crate::core::data::{Transform, sanitize_filename, unique_path, yaml_quote};

/// The two dedup dotfiles' names (per ADR-0012), anchored here since this
/// module is their conceptual owner -- the generic `ContentIndex` type
/// itself (ADR-0020) no longer hardcodes any filename. `EmailDedup`
/// (`dedup.rs`) is now their only consumer.
pub(crate) const MESSAGE_HASHES_FILE: &str = ".message-hashes";
pub(crate) const ATTACHMENT_HASHES_FILE: &str = ".attachment-hashes";

/// One attachment staged by `EmailTransform::transform`, not yet placed at
/// its final, possibly-deduped location -- that happens in the post-
/// transform dedup pass (ADR-0021 §7/§10).
pub(crate) struct StagedAttachment {
    pub hash: String,
    pub staged_path: PathBuf,
    /// `attachments/<name>`, relative to the staged message's own directory
    /// -- the same relative-naming convention used for the final,
    /// identity-rooted flat tree (ADR-0006), so the dedup pass can reuse it
    /// unchanged once a canonical location is chosen.
    pub staged_relpath: String,
}

/// What a single `EmailTransform::transform` call wrote, entirely under a
/// UID-keyed staging tree (`<staging_root>/transformed/<mailbox-relpath>/
/// ...`) rather than the final flat identity tree. `(mailbox, uid)` is
/// unique per IMAP's own guarantees and exclusive to whichever worker
/// fetched it, so nothing here is ever contended by another concurrent
/// worker -- no lock is needed (ADR-0021 §7/§10). Placing this content at
/// its final, deduped location and resolving any real content-hash
/// duplicates is entirely the single-threaded post-transform dedup pass's
/// job.
pub(crate) struct TransformOutcome {
    pub message_hash: String,
    pub md_staged_path: PathBuf,
    /// Staging-root-relative path (e.g. `transformed/inbox/5.md`), for
    /// persisting in a `manifest::CheckpointEntry`.
    pub md_staged_relpath: String,
    /// The human-readable, date+subject-derived filename (e.g.
    /// `2024-01-26-hello-world.md`) this message would be named at its
    /// final, identity-rooted location (ADR-0006) -- computed here (where
    /// the parsed subject/date are already in hand) so the post-transform
    /// dedup pass doesn't need to re-derive it. The staged file itself is
    /// named `<uid>.md` instead (collision-free by construction, since UIDs
    /// are unique per mailbox); this field is only consulted once, by the
    /// dedup pass's `unique_path` call against the shared final tree.
    pub desired_md_name: String,
    /// The `mailbox/...` tag this message was written with (e.g.
    /// `mailbox/archive/2020`) -- the same string embedded in its own
    /// `tags:` frontmatter. Carried through so the post-transform dedup
    /// pass can call `amend_frontmatter_for_duplicate(canonical, tag,
    /// occurrence)` for a duplicate without re-deriving it from a raw IMAP
    /// mailbox name (which would need that mailbox's delimiter on hand).
    pub mailbox_tag: String,
    pub attachments: Vec<StagedAttachment>,
}

/// The email-specific implementation of `core::data::Transform` (ADR-0023):
/// parses a single `.eml` file and unconditionally stages its Markdown
/// rendering (and any attachments) under `staging_root`'s UID-keyed tree.
/// `input_root` is used to derive the `mailbox/...` tag and the staged
/// tree's mirrored mailbox subdirectory from the given `.eml` path's
/// location relative to it. Constructed once per worker/batch and reused
/// across every UID it processes (`identity`/`input_root`/`staging_root`
/// never change mid-batch).
pub(crate) struct EmailTransform {
    pub identity: Identity,
    pub input_root: PathBuf,
    pub staging_root: PathBuf,
}

impl Transform for EmailTransform {
    type Input = PathBuf;
    type Output = TransformOutcome;

    /// Returns `Ok(None)` on a lenient skip (unparseable message, missing
    /// `Date` header, or a filename that doesn't parse as a `u32` UID --
    /// needed for the `uid:` frontmatter field) with a warning already
    /// printed; `Err` only for a hard I/O failure.
    fn transform(&self, eml_path: PathBuf) -> Result<Option<TransformOutcome>, String> {
        let Some(uid) = eml_path
            .file_stem()
            .and_then(|stem| stem.to_str())
            .and_then(|stem| stem.parse::<u32>().ok())
        else {
            eprintln!(
                "Warning: {} is not named <uid>.eml, skipping",
                eml_path.display()
            );
            return Ok(None);
        };

        let bytes = match fs::read(&eml_path) {
            Ok(bytes) => bytes,
            Err(err) => {
                eprintln!("Warning: failed to read {}: {err}", eml_path.display());
                return Ok(None);
            }
        };

        let message_hash = format!("{:x}", md5::compute(&bytes));

        let Some(message) = MessageParser::default().parse(&bytes) else {
            eprintln!("Warning: failed to parse {}, skipping", eml_path.display());
            return Ok(None);
        };

        let Some(date) = message.date() else {
            eprintln!(
                "Warning: {} has no Date header, skipping",
                eml_path.display()
            );
            return Ok(None);
        };

        let mailbox = mailbox_tag(&eml_path, &self.input_root);
        let relative_dir = eml_path
            .strip_prefix(&self.input_root)
            .ok()
            .and_then(|path| path.parent())
            .map(Path::to_path_buf)
            .unwrap_or_default();
        let staged_dir = self.staging_root.join("transformed").join(&relative_dir);
        fs::create_dir_all(&staged_dir)
            .map_err(|err| format!("failed to create {}: {err}", staged_dir.display()))?;

        let subject = message.subject().unwrap_or("(no subject)");
        let from_addr = message.from().and_then(|address| address.first());
        let to_addr = message.to().and_then(|address| address.first());

        let body = if message.html_body_count() > 0 {
            let html = message.body_html(0).unwrap_or_default();
            htmd::convert(&html)
                .unwrap_or_else(|_| message.body_text(0).unwrap_or_default().to_string())
        } else if message.text_body_count() > 0 {
            message.body_text(0).unwrap_or_default().to_string()
        } else {
            String::new()
        };

        let stem = format!(
            "{}-{}",
            format_date_prefix(date),
            identity::sanitize_segment(subject)
        );

        // A UID's staging subtree is exclusively this worker's -- no other
        // concurrent worker ever writes into it (batches never share a
        // UID) -- so `unique_path` here only ever resolves a genuine
        // within-message naming collision (e.g. two attachments both
        // literally named "image.png"), never a cross-worker race. The
        // durable, content-hash-based dedup decision, and its own
        // `unique_path` call against the shared final tree, happen only in
        // the single-threaded post-transform pass (ADR-0021 §7/§10).
        let attachments_dir = staged_dir.join(uid.to_string()).join("attachments");
        let mut attachments = Vec::new();
        let mut attachment_relpaths = Vec::new();
        for part in message.attachments() {
            let contents = part.contents();
            let name = part.attachment_name();
            if contents.is_empty() && name.is_none() {
                // A truncated/malformed trailing MIME part with no headers
                // and no content -- not a real attachment, just an
                // artifact of a malformed source message (ADR-0030
                // amendment). Staging it as a 0-byte file would make
                // `verify_transformed` reject the entire message over a
                // phantom part it never actually sent.
                continue;
            }
            let hash = format!("{:x}", md5::compute(contents));
            fs::create_dir_all(&attachments_dir)
                .map_err(|err| format!("failed to create {}: {err}", attachments_dir.display()))?;

            let original_name = sanitize_filename(name.unwrap_or("attachment"));
            let staged_path = unique_path(&attachments_dir.join(format!("{stem}-{original_name}")));
            fs::write(&staged_path, contents)
                .map_err(|err| format!("failed to write {}: {err}", staged_path.display()))?;

            let relpath = format!(
                "attachments/{}",
                staged_path.file_name().unwrap().to_string_lossy()
            );
            attachments.push(StagedAttachment {
                hash,
                staged_path,
                staged_relpath: relpath.clone(),
            });
            attachment_relpaths.push(relpath);
        }

        let mut tags = vec![
            mailbox.clone(),
            format!("identity/{}", self.identity.alias),
            format!("year/{}", date.year),
        ];
        if let Some(domain_tag) =
            sender_domain_tag(from_addr.and_then(|addr| addr.address.as_deref()))
        {
            tags.push(domain_tag);
        }

        let frontmatter = render_frontmatter(
            &format_address(from_addr),
            &format_address(to_addr),
            subject,
            &date.to_rfc3339(),
            &tags,
            &attachment_relpaths,
            uid,
        );

        let md_staged_path = staged_dir.join(format!("{uid}.md"));
        fs::write(&md_staged_path, format!("{frontmatter}\n{body}"))
            .map_err(|err| format!("failed to write {}: {err}", md_staged_path.display()))?;
        let md_staged_relpath = relpath_string(&self.staging_root, &md_staged_path);

        Ok(Some(TransformOutcome {
            message_hash,
            md_staged_path,
            md_staged_relpath,
            desired_md_name: format!("{stem}.md"),
            mailbox_tag: mailbox,
            attachments,
        }))
    }
}

/// Why `verify_transformed` rejected an outcome (ADR-0033 #37/#38) --
/// carries enough detail for the worker's per-UID warning and the job-level
/// `FailureBreakdown` count to distinguish structural verification failure
/// from the other four failure categories.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum VerifyFailure {
    MarkdownMissingOrEmpty,
    FrontmatterDelimiterMissing,
    AttachmentMissingOrEmpty(String),
}

impl std::fmt::Display for VerifyFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            VerifyFailure::MarkdownMissingOrEmpty => {
                write!(f, "staged markdown file is missing or empty")
            }
            VerifyFailure::FrontmatterDelimiterMissing => {
                write!(
                    f,
                    "staged markdown file is missing its frontmatter delimiter"
                )
            }
            VerifyFailure::AttachmentMissingOrEmpty(relpath) => {
                write!(f, "staged attachment '{relpath}' is missing or empty")
            }
        }
    }
}

/// Structural check that `EmailTransform::transform`'s output is complete:
/// the staged `.md` file exists, is non-empty, and starts with the
/// frontmatter delimiter; every staged attachment path exists with nonzero
/// size. Used by the worker pool to decide whether it's safe to delete the
/// source `.eml` and record the UID as checkpointed.
pub(crate) fn verify_transformed(outcome: &TransformOutcome) -> Result<(), VerifyFailure> {
    let Ok(contents) = fs::read(&outcome.md_staged_path) else {
        return Err(VerifyFailure::MarkdownMissingOrEmpty);
    };
    if contents.is_empty() {
        return Err(VerifyFailure::MarkdownMissingOrEmpty);
    }
    if !contents.starts_with(b"---") {
        return Err(VerifyFailure::FrontmatterDelimiterMissing);
    }
    for attachment in &outcome.attachments {
        if !fs::metadata(&attachment.staged_path).is_ok_and(|meta| meta.len() > 0) {
            return Err(VerifyFailure::AttachmentMissingOrEmpty(
                attachment.staged_relpath.clone(),
            ));
        }
    }
    Ok(())
}

/// `path`'s location relative to `base`, joined with `/` regardless of the
/// host platform's path separator -- the same component-wise-join technique
/// `email::sync::upload_key` used for S3 keys, reused here for staging-
/// root-relative checkpoint paths.
fn relpath_string(base: &Path, path: &Path) -> String {
    path.strip_prefix(base)
        .unwrap_or(path)
        .components()
        .map(|component| component.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}

fn format_address(addr: Option<&Addr>) -> String {
    let Some(addr) = addr else {
        return String::new();
    };
    match (addr.name.as_deref(), addr.address.as_deref()) {
        (Some(name), Some(email)) => format!("{name} <{email}>"),
        (None, Some(email)) => email.to_string(),
        (Some(name), None) => name.to_string(),
        (None, None) => String::new(),
    }
}

/// The `.eml` file's parent directory path relative to `input_root`, joined
/// with `/` and prefixed `mailbox/` (e.g. `mailbox/archive/2020`). Sink
/// (ADR-0005) already sanitizes these directory names, so no further
/// sanitization happens here.
fn mailbox_tag(eml_path: &Path, input_root: &Path) -> String {
    let relative_dir = eml_path
        .strip_prefix(input_root)
        .ok()
        .and_then(|path| path.parent())
        .unwrap_or_else(|| Path::new(""));
    let joined: Vec<String> = relative_dir
        .components()
        .map(|component| component.as_os_str().to_string_lossy().into_owned())
        .collect();
    format!("mailbox/{}", joined.join("/"))
}

/// `sender/<domain>` from an address string, domain sanitized the same way
/// as every other path/tag segment.
fn sender_domain_tag(address: Option<&str>) -> Option<String> {
    let domain = address.and_then(|addr| addr.rsplit('@').next())?;
    Some(format!("sender/{}", identity::sanitize_segment(domain)))
}

fn format_date_prefix(date: &DateTime) -> String {
    format!("{:04}-{:02}-{:02}", date.year, date.month, date.day)
}

fn render_frontmatter(
    from: &str,
    to: &str,
    subject: &str,
    date_rfc3339: &str,
    tags: &[String],
    attachments: &[String],
    uid: u32,
) -> String {
    let mut out = String::from("---\n");
    out.push_str(&format!("from: {}\n", yaml_quote(from)));
    out.push_str(&format!("to: {}\n", yaml_quote(to)));
    out.push_str(&format!("subject: {}\n", yaml_quote(subject)));
    out.push_str(&format!("date: {date_rfc3339}\n"));
    out.push_str("tags:\n");
    for tag in tags {
        out.push_str(&format!("  - {tag}\n"));
    }
    if !attachments.is_empty() {
        out.push_str("attachments:\n");
        for attachment in attachments {
            out.push_str(&format!("  - {attachment}\n"));
        }
    }
    out.push_str(&format!("uid: {uid}\n"));
    out.push_str("---\n");
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mailbox_tag_joins_nested_path() {
        assert_eq!(
            mailbox_tag(
                Path::new("/tmp/in/archive/2020/5.eml"),
                Path::new("/tmp/in")
            ),
            "mailbox/archive/2020"
        );
    }

    #[test]
    fn mailbox_tag_single_level() {
        assert_eq!(
            mailbox_tag(Path::new("/tmp/in/inbox/1.eml"), Path::new("/tmp/in")),
            "mailbox/inbox"
        );
    }

    #[test]
    fn sender_domain_tag_extracts_and_sanitizes_domain() {
        assert_eq!(
            sender_domain_tag(Some("jane.doe@example.com")),
            Some("sender/example-com".to_string())
        );
    }

    #[test]
    fn sender_domain_tag_none_when_no_address() {
        assert_eq!(sender_domain_tag(None), None);
    }

    #[test]
    fn format_date_prefix_pads_single_digits() {
        let date = DateTime::parse_rfc822("Fri, 26 Jan 2024 09:15:00 +0000").unwrap();
        assert_eq!(format_date_prefix(&date), "2024-01-26");
    }

    #[test]
    fn render_frontmatter_matches_expected_shape() {
        let out = render_frontmatter(
            "Jane Doe <jane.doe@example.com>",
            "first.last@example.com",
            "Hello, World!",
            "2024-01-26T09:15:00+00:00",
            &[
                "mailbox/inbox".to_string(),
                "identity/first-last".to_string(),
            ],
            &["attachments/2024-01-26-hello-world-bingo.pdf".to_string()],
            482,
        );
        assert_eq!(
            out,
            "---\n\
             from: \"Jane Doe <jane.doe@example.com>\"\n\
             to: \"first.last@example.com\"\n\
             subject: \"Hello, World!\"\n\
             date: 2024-01-26T09:15:00+00:00\n\
             tags:\n\
             \x20\x20- mailbox/inbox\n\
             \x20\x20- identity/first-last\n\
             attachments:\n\
             \x20\x20- attachments/2024-01-26-hello-world-bingo.pdf\n\
             uid: 482\n\
             ---\n"
        );
    }

    fn dummy_outcome(
        md_staged_path: PathBuf,
        attachments: Vec<StagedAttachment>,
    ) -> TransformOutcome {
        TransformOutcome {
            message_hash: "irrelevant".to_string(),
            md_staged_path,
            md_staged_relpath: "irrelevant".to_string(),
            desired_md_name: "irrelevant.md".to_string(),
            mailbox_tag: "mailbox/irrelevant".to_string(),
            attachments,
        }
    }

    #[test]
    fn verify_transformed_true_for_valid_output() {
        let dir = tempfile::tempdir().unwrap();
        let md_path = dir.path().join("5.md");
        fs::write(&md_path, "---\nfrom: \"a\"\n---\nbody").unwrap();
        let attachment_path = dir.path().join("5").join("attachments").join("a.pdf");
        fs::create_dir_all(attachment_path.parent().unwrap()).unwrap();
        fs::write(&attachment_path, b"content").unwrap();

        assert!(
            verify_transformed(&dummy_outcome(
                md_path,
                vec![StagedAttachment {
                    hash: "h".to_string(),
                    staged_path: attachment_path,
                    staged_relpath: "attachments/a.pdf".to_string(),
                }]
            ))
            .is_ok()
        );
    }

    #[test]
    fn verify_transformed_false_when_md_missing() {
        let dir = tempfile::tempdir().unwrap();
        let md_path = dir.path().join("does-not-exist.md");

        assert_eq!(
            verify_transformed(&dummy_outcome(md_path, vec![])),
            Err(VerifyFailure::MarkdownMissingOrEmpty)
        );
    }

    #[test]
    fn verify_transformed_false_when_attachment_missing() {
        let dir = tempfile::tempdir().unwrap();
        let md_path = dir.path().join("5.md");
        fs::write(&md_path, "---\nfrom: \"a\"\n---\nbody").unwrap();

        assert_eq!(
            verify_transformed(&dummy_outcome(
                md_path,
                vec![StagedAttachment {
                    hash: "h".to_string(),
                    staged_path: dir.path().join("missing.pdf"),
                    staged_relpath: "attachments/missing.pdf".to_string(),
                }]
            )),
            Err(VerifyFailure::AttachmentMissingOrEmpty(
                "attachments/missing.pdf".to_string()
            ))
        );
    }

    fn test_identity() -> Identity {
        Identity {
            alias: "first-last".to_string(),
            email: "first.last@example.com".to_string(),
            provider: crate::commands::keyring::email::provider::Provider::Gmail,
            host: "imap.gmail.com".to_string(),
            port: 993,
        }
    }

    fn transformer(input_root: &Path, staging_root: &Path) -> EmailTransform {
        EmailTransform {
            identity: test_identity(),
            input_root: input_root.to_path_buf(),
            staging_root: staging_root.to_path_buf(),
        }
    }

    fn plain_text_eml(subject: &str) -> String {
        format!(
            "From: Jane Doe <jane.doe@example.com>\r\n\
             To: first.last@example.com\r\n\
             Subject: {subject}\r\n\
             Date: Fri, 26 Jan 2024 09:15:00 +0000\r\n\
             Content-Type: text/plain; charset=utf-8\r\n\
             \r\n\
             Hello there!\r\n"
        )
    }

    fn eml_with_attachment(subject: &str, filename: &str, attachment_base64: &str) -> String {
        format!(
            "From: Jane Doe <jane.doe@example.com>\r\n\
             To: first.last@example.com\r\n\
             Subject: {subject}\r\n\
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
             Content-Disposition: attachment; filename=\"{filename}\"\r\n\
             Content-Transfer-Encoding: base64\r\n\
             \r\n\
             {attachment_base64}\r\n\
             --BOUNDARY--\r\n"
        )
    }

    #[test]
    fn transform_one_writes_to_uid_keyed_staging_tree() {
        let staging = tempfile::tempdir().unwrap();
        let input = tempfile::tempdir().unwrap();
        let inbox = input.path().join("inbox");
        fs::create_dir_all(&inbox).unwrap();
        fs::write(inbox.join("5.eml"), plain_text_eml("Hello")).unwrap();

        let outcome = transformer(input.path(), staging.path())
            .transform(inbox.join("5.eml"))
            .unwrap()
            .unwrap();

        assert_eq!(
            outcome.md_staged_path,
            staging
                .path()
                .join("transformed")
                .join("inbox")
                .join("5.md")
        );
        assert_eq!(outcome.md_staged_relpath, "transformed/inbox/5.md");
        assert!(fs::metadata(&outcome.md_staged_path).is_ok());
    }

    #[test]
    fn transform_one_does_not_dedupe_byte_identical_messages() {
        let staging = tempfile::tempdir().unwrap();
        let input = tempfile::tempdir().unwrap();
        let inbox = input.path().join("inbox");
        let archive = input.path().join("archive");
        fs::create_dir_all(&inbox).unwrap();
        fs::create_dir_all(&archive).unwrap();

        let raw = plain_text_eml("Hello");
        fs::write(inbox.join("1.eml"), &raw).unwrap();
        fs::write(archive.join("2.eml"), &raw).unwrap();

        let transformer = transformer(input.path(), staging.path());
        let first = transformer.transform(inbox.join("1.eml")).unwrap().unwrap();
        let second = transformer
            .transform(archive.join("2.eml"))
            .unwrap()
            .unwrap();

        // No dedup at this layer anymore -- both are staged independently,
        // as separate files, even though their content (and hash) is
        // identical. Merging is entirely the post-transform dedup pass's
        // job now.
        assert_eq!(first.message_hash, second.message_hash);
        assert_ne!(first.md_staged_path, second.md_staged_path);
        assert!(fs::metadata(&first.md_staged_path).is_ok());
        assert!(fs::metadata(&second.md_staged_path).is_ok());
    }

    #[test]
    fn transform_one_disambiguates_same_named_attachments_within_one_message() {
        let staging = tempfile::tempdir().unwrap();
        let input = tempfile::tempdir().unwrap();
        let inbox = input.path().join("inbox");
        fs::create_dir_all(&inbox).unwrap();
        fs::write(
            inbox.join("1.eml"),
            eml_with_attachment("Shipping", "a.pdf", "JVBERi0xLjQK"),
        )
        .unwrap();

        let outcome = transformer(input.path(), staging.path())
            .transform(inbox.join("1.eml"))
            .unwrap()
            .unwrap();

        assert_eq!(outcome.attachments.len(), 1);
        assert!(fs::metadata(&outcome.attachments[0].staged_path).is_ok_and(|m| m.len() > 0));
    }

    #[test]
    fn transform_one_sanitizes_attachment_name_with_path_separators() {
        let staging = tempfile::tempdir().unwrap();
        let input = tempfile::tempdir().unwrap();
        let inbox = input.path().join("inbox");
        fs::create_dir_all(&inbox).unwrap();
        fs::write(
            inbox.join("1.eml"),
            eml_with_attachment("Shipping", "/img0.png", "JVBERi0xLjQK"),
        )
        .unwrap();

        let outcome = transformer(input.path(), staging.path())
            .transform(inbox.join("1.eml"))
            .unwrap()
            .unwrap();

        let attachment_path = &outcome.attachments[0].staged_path;
        assert!(
            !attachment_path
                .file_name()
                .unwrap()
                .to_string_lossy()
                .contains('/')
        );
        assert!(fs::metadata(attachment_path).is_ok_and(|meta| meta.len() > 0));
    }

    #[test]
    fn transform_one_truncates_a_very_long_subject() {
        let staging = tempfile::tempdir().unwrap();
        let input = tempfile::tempdir().unwrap();
        let inbox = input.path().join("inbox");
        fs::create_dir_all(&inbox).unwrap();

        // Long enough that the unpatched code would build a >255-byte
        // filename and fail to write with ENAMETOOLONG.
        let long_subject = "word ".repeat(60);
        fs::write(inbox.join("1.eml"), plain_text_eml(&long_subject)).unwrap();

        let outcome = transformer(input.path(), staging.path())
            .transform(inbox.join("1.eml"))
            .unwrap()
            .unwrap();

        // The staged file itself is always named `<uid>.md`, so subject
        // length can't affect it -- but `desired_md_name` (what the dedup
        // pass will eventually name the final file) is subject-derived and
        // must still be truncated to a safe length.
        assert!(outcome.desired_md_name.len() <= 255);
        assert!(fs::metadata(&outcome.md_staged_path).is_ok());
    }

    /// Regression coverage for the ADR-0030 amendment (Finding 1): a
    /// malformed source message (mirroring real messages seen in
    /// production -- a `multipart/mixed` body whose second part opens a
    /// boundary line but never has headers, content, or a closing
    /// terminator, i.e. the raw message is truncated) must not have its
    /// real content rejected just because `mail_parser` exposes that
    /// dangling part as a phantom, nameless, zero-byte "attachment."
    #[test]
    fn transform_one_skips_a_phantom_empty_nameless_attachment_part() {
        let staging = tempfile::tempdir().unwrap();
        let input = tempfile::tempdir().unwrap();
        let inbox = input.path().join("inbox");
        fs::create_dir_all(&inbox).unwrap();

        let eml = "From: Jane Doe <jane.doe@example.com>\r\n\
            To: first.last@example.com\r\n\
            Subject: Receipt\r\n\
            Date: Fri, 26 Jan 2024 09:15:00 +0000\r\n\
            MIME-Version: 1.0\r\n\
            Content-Type: multipart/mixed; boundary=BOUNDARY\r\n\
            \r\n\
            --BOUNDARY\r\n\
            Content-Type: text/html; charset=UTF-8\r\n\
            \r\n\
            <html><body>Receipt</body></html>\r\n\
            --BOUNDARY\r\n";
        fs::write(inbox.join("1.eml"), eml).unwrap();

        let outcome = transformer(input.path(), staging.path())
            .transform(inbox.join("1.eml"))
            .unwrap()
            .unwrap();

        assert!(
            outcome.attachments.is_empty(),
            "the dangling, header-less trailing part should not be staged as an attachment"
        );
        assert!(verify_transformed(&outcome).is_ok());
    }
}
