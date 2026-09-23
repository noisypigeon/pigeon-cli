use std::fs;
use std::path::{Path, PathBuf};

use mail_parser::{Addr, DateTime, MessageParser, MimeHeaders};

use crate::email::dedup::{self, ContentIndex};
use crate::email::identity::{self, Identity};

/// Summary of a completed `transform` run.
#[derive(Debug, Default)]
pub struct TransformSummary {
    pub messages: usize,
    pub attachments: usize,
    pub skipped: usize,
    pub merged_messages: usize,
    pub deduped_attachments: usize,
}

/// The artifacts a successful `transform_one` call wrote, needed by
/// `verify_transformed` (and, per ADR-0007, by `sync`'s verify-then-delete
/// step), plus ADR-0012's dedup bookkeeping.
pub(crate) struct TransformedMessage {
    pub md_path: PathBuf,
    /// Every attachment this message references -- both freshly written
    /// ones and ones deduped against an existing canonical file. Kept
    /// together (not split by new/reused) so `verify_transformed` and
    /// `sync`'s upload logic (ADR-0011) need no changes: a reused path
    /// already exists on disk, and its upload naturally reports unchanged.
    pub attachment_paths: Vec<PathBuf>,
    /// True if this call merged a duplicate whole message into an existing
    /// canonical file rather than writing anything new (ADR-0012).
    pub merged: bool,
    /// True only when `merged` and the canonical file's frontmatter was
    /// actually rewritten (a new mailbox/uid combination). False for a
    /// fresh (non-merged) message, and false for an idempotent replay of an
    /// already-recorded duplicate.
    pub canonical_frontmatter_changed: bool,
    /// Count of this message's attachments that hit the attachment-hash
    /// index instead of being freshly written.
    pub attachments_deduped: usize,
    /// `(hash, identity-dir-relative path)` for this message's own raw
    /// bytes, for the caller to commit to the message-hash index if this
    /// result is accepted (e.g. after verification passes). `None` for a
    /// merge -- there's nothing new to record.
    pub pending_message_hash: Option<(String, String)>,
    /// `(hash, identity-dir-relative path)` pairs for newly written (not
    /// deduped) attachments, for the caller to commit to the
    /// attachment-hash index if this result is accepted.
    pub pending_attachment_hashes: Vec<(String, String)>,
}

/// Parses every `.eml` file under `input` (as produced by `sink`, per
/// ADR-0005) into a flat, per-identity Markdown tree with YAML frontmatter
/// under `output`, per ADR-0006. Read-only over `input` -- nothing sunk is
/// ever modified or deleted.
///
/// This is `pigeon email sync --debug transform`'s implementation
/// (ADR-0007) -- transform-only, never fetches, never deletes the source
/// `.eml`.
pub fn run(identity: &Identity, input: &Path, output: &Path) -> Result<TransformSummary, String> {
    let eml_files = find_eml_files(input)?;

    let mut message_index = ContentIndex::load(input, ContentIndex::MESSAGE_HASHES)?;
    let mut attachment_index = ContentIndex::load(input, ContentIndex::ATTACHMENT_HASHES)?;

    let mut summary = TransformSummary::default();

    for eml_path in &eml_files {
        match transform_one(
            identity,
            eml_path,
            input,
            output,
            &message_index,
            &attachment_index,
        )? {
            Some(transformed) => {
                // No verify gate in this mode (ADR-0007), so a successful
                // write's dedup entries are committed immediately.
                if let Some((hash, relpath)) = &transformed.pending_message_hash {
                    message_index.commit(input, hash, relpath)?;
                }
                for (hash, relpath) in &transformed.pending_attachment_hashes {
                    attachment_index.commit(input, hash, relpath)?;
                }

                if transformed.merged {
                    summary.merged_messages += 1;
                } else {
                    summary.messages += 1;
                }
                summary.attachments +=
                    transformed.attachment_paths.len() - transformed.attachments_deduped;
                summary.deduped_attachments += transformed.attachments_deduped;
            }
            None => summary.skipped += 1,
        }
    }

    Ok(summary)
}

/// Parses a single `.eml` file and writes its Markdown (and any attachments)
/// under `output`. `input_root` is used to derive the `mailbox/...` tag from
/// `eml_path`'s location relative to it.
///
/// Returns `Ok(None)` on a lenient skip (unparseable message, missing `Date`
/// header, or a filename that doesn't parse as a `u32` UID -- needed for the
/// `uid:` frontmatter field) with a warning already printed; `Err` only for
/// a hard I/O failure.
pub(crate) fn transform_one(
    identity: &Identity,
    eml_path: &Path,
    input_root: &Path,
    output: &Path,
    message_index: &ContentIndex,
    attachment_index: &ContentIndex,
) -> Result<Option<TransformedMessage>, String> {
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

    let bytes = match fs::read(eml_path) {
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

    let mailbox = mailbox_tag(eml_path, input_root);
    let identity_dir = output.join(identity::sanitize_segment(&identity.email));

    // ADR-0012: the exact same physical message exposed at a second
    // (mailbox, uid) pair (e.g. a provider exposing one message through
    // more than one mailbox) gets merged into the already-canonical `.md`
    // instead of writing a duplicate file.
    if let Some(canonical_relpath) = message_index.check(&message_hash) {
        let canonical_md_path = identity_dir.join(canonical_relpath);
        return match dedup::amend_frontmatter_for_duplicate(&canonical_md_path, &mailbox, uid) {
            Ok(changed) => Ok(Some(TransformedMessage {
                md_path: canonical_md_path,
                attachment_paths: Vec::new(),
                merged: true,
                canonical_frontmatter_changed: changed,
                attachments_deduped: 0,
                pending_message_hash: None,
                pending_attachment_hashes: Vec::new(),
            })),
            Err(err) => {
                eprintln!(
                    "Warning: canonical file for duplicate {} is missing or malformed: {err}, skipping",
                    eml_path.display()
                );
                Ok(None)
            }
        };
    }

    let attachments_dir = identity_dir.join("attachments");
    fs::create_dir_all(&attachments_dir)
        .map_err(|err| format!("failed to create {}: {err}", attachments_dir.display()))?;

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

    let mut attachment_paths = Vec::new();
    let mut attachment_relpaths = Vec::new();
    let mut pending_attachment_hashes: Vec<(String, String)> = Vec::new();
    let mut attachments_deduped = 0usize;
    for part in message.attachments() {
        let contents = part.contents();
        let hash = format!("{:x}", md5::compute(contents));

        // ADR-0012: check both the durable index and this message's own
        // not-yet-committed attachments, so two identical attachments
        // within the same message dedupe against each other too.
        let existing = attachment_index
            .check(&hash)
            .map(str::to_string)
            .or_else(|| {
                pending_attachment_hashes
                    .iter()
                    .find(|(existing_hash, _)| existing_hash == &hash)
                    .map(|(_, relpath)| relpath.clone())
            });

        let relpath = match existing {
            Some(relpath) => {
                attachments_deduped += 1;
                attachment_paths.push(identity_dir.join(&relpath));
                relpath
            }
            None => {
                let original_name = part.attachment_name().unwrap_or("attachment");
                let attachment_path =
                    unique_path(&attachments_dir.join(format!("{stem}-{original_name}")));
                fs::write(&attachment_path, contents).map_err(|err| {
                    format!("failed to write {}: {err}", attachment_path.display())
                })?;
                let relpath = format!(
                    "attachments/{}",
                    attachment_path.file_name().unwrap().to_string_lossy()
                );
                attachment_paths.push(attachment_path);
                pending_attachment_hashes.push((hash, relpath.clone()));
                relpath
            }
        };
        attachment_relpaths.push(relpath);
    }

    let mut tags = vec![
        mailbox,
        format!("identity/{}", identity.alias),
        format!("year/{}", date.year),
    ];
    if let Some(domain_tag) = sender_domain_tag(from_addr.and_then(|addr| addr.address.as_deref()))
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

    let md_path = unique_path(&identity_dir.join(format!("{stem}.md")));
    fs::write(&md_path, format!("{frontmatter}\n{body}"))
        .map_err(|err| format!("failed to write {}: {err}", md_path.display()))?;

    let pending_message_hash = Some((
        message_hash,
        md_path.file_name().unwrap().to_string_lossy().into_owned(),
    ));

    Ok(Some(TransformedMessage {
        md_path,
        attachment_paths,
        merged: false,
        canonical_frontmatter_changed: false,
        attachments_deduped,
        pending_message_hash,
        pending_attachment_hashes,
    }))
}

/// Structural check that `transform_one`'s output is complete: the `.md`
/// file exists, is non-empty, and starts with the frontmatter delimiter;
/// every attachment path it wrote exists with nonzero size. Used by `sync`
/// (ADR-0007) to decide whether it's safe to delete the source `.eml`.
pub(crate) fn verify_transformed(transformed: &TransformedMessage) -> bool {
    let Ok(contents) = fs::read(&transformed.md_path) else {
        return false;
    };
    if contents.is_empty() || !contents.starts_with(b"---") {
        return false;
    }
    transformed
        .attachment_paths
        .iter()
        .all(|path| fs::metadata(path).is_ok_and(|meta| meta.len() > 0))
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

/// Recursively collects every `*.eml` path under `input`, sorted for
/// deterministic output.
fn find_eml_files(input: &Path) -> Result<Vec<PathBuf>, String> {
    let mut files = Vec::new();
    visit_dir(input, &mut files)?;
    files.sort();
    Ok(files)
}

fn visit_dir(dir: &Path, files: &mut Vec<PathBuf>) -> Result<(), String> {
    let entries =
        fs::read_dir(dir).map_err(|err| format!("failed to read {}: {err}", dir.display()))?;
    for entry in entries {
        let entry = entry.map_err(|err| format!("failed to read {}: {err}", dir.display()))?;
        let path = entry.path();
        if path.is_dir() {
            visit_dir(&path, files)?;
        } else if path.extension().and_then(|ext| ext.to_str()) == Some("eml") {
            files.push(path);
        }
    }
    Ok(())
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

/// Escapes `s` as a double-quoted YAML scalar.
fn yaml_quote(s: &str) -> String {
    let escaped = s.replace('\\', "\\\\").replace('"', "\\\"");
    format!("\"{escaped}\"")
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

/// If `desired` doesn't exist yet, returns it as-is; otherwise appends
/// `-2`, `-3`, ... before the extension until a free path is found.
fn unique_path(desired: &Path) -> PathBuf {
    if !desired.exists() {
        return desired.to_path_buf();
    }
    let stem = desired
        .file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or("file");
    let ext = desired.extension().and_then(|ext| ext.to_str());
    let parent = desired.parent().unwrap_or_else(|| Path::new(""));

    let mut n = 2;
    loop {
        let candidate_name = match ext {
            Some(ext) => format!("{stem}-{n}.{ext}"),
            None => format!("{stem}-{n}"),
        };
        let candidate = parent.join(candidate_name);
        if !candidate.exists() {
            return candidate;
        }
        n += 1;
    }
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
    fn yaml_quote_escapes_quotes_and_backslashes() {
        assert_eq!(yaml_quote("Hello: World"), "\"Hello: World\"");
        assert_eq!(yaml_quote(r#"She said "hi""#), r#""She said \"hi\"""#);
    }

    #[test]
    fn unique_path_returns_original_when_free() {
        let dir = tempfile::tempdir().unwrap();
        let desired = dir.path().join("2024-01-26-hello.md");
        assert_eq!(unique_path(&desired), desired);
    }

    #[test]
    fn unique_path_suffixes_on_collision() {
        let dir = tempfile::tempdir().unwrap();
        let desired = dir.path().join("2024-01-26-hello.md");
        fs::write(&desired, b"").unwrap();

        let resolved = unique_path(&desired);
        assert_eq!(resolved, dir.path().join("2024-01-26-hello-2.md"));
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

    /// Builds a `TransformedMessage` for `verify_transformed` tests, which
    /// only care about `md_path`/`attachment_paths` -- the ADR-0012 dedup
    /// bookkeeping fields are irrelevant there.
    fn dummy_transformed(md_path: PathBuf, attachment_paths: Vec<PathBuf>) -> TransformedMessage {
        TransformedMessage {
            md_path,
            attachment_paths,
            merged: false,
            canonical_frontmatter_changed: false,
            attachments_deduped: 0,
            pending_message_hash: None,
            pending_attachment_hashes: Vec::new(),
        }
    }

    #[test]
    fn verify_transformed_true_for_valid_output() {
        let dir = tempfile::tempdir().unwrap();
        let md_path = dir.path().join("2024-01-26-hello.md");
        fs::write(&md_path, "---\nfrom: \"a\"\n---\nbody").unwrap();
        let attachment_path = dir.path().join("attachments").join("a.pdf");
        fs::create_dir_all(attachment_path.parent().unwrap()).unwrap();
        fs::write(&attachment_path, b"content").unwrap();

        assert!(verify_transformed(&dummy_transformed(
            md_path,
            vec![attachment_path]
        )));
    }

    #[test]
    fn verify_transformed_false_when_md_missing() {
        let dir = tempfile::tempdir().unwrap();
        let md_path = dir.path().join("does-not-exist.md");

        assert!(!verify_transformed(&dummy_transformed(md_path, vec![])));
    }

    #[test]
    fn verify_transformed_false_when_attachment_missing() {
        let dir = tempfile::tempdir().unwrap();
        let md_path = dir.path().join("2024-01-26-hello.md");
        fs::write(&md_path, "---\nfrom: \"a\"\n---\nbody").unwrap();

        assert!(!verify_transformed(&dummy_transformed(
            md_path,
            vec![dir.path().join("attachments").join("missing.pdf")]
        )));
    }

    #[test]
    fn verify_transformed_true_for_merged_message_with_no_own_attachments() {
        let dir = tempfile::tempdir().unwrap();
        let canonical_path = dir.path().join("2024-01-26-hello.md");
        fs::write(&canonical_path, "---\nfrom: \"a\"\n---\nbody").unwrap();

        let mut transformed = dummy_transformed(canonical_path, vec![]);
        transformed.merged = true;
        transformed.canonical_frontmatter_changed = true;

        assert!(verify_transformed(&transformed));
    }

    #[test]
    fn transform_one_dedupes_repeated_attachment_within_a_run() {
        let staging = tempfile::tempdir().unwrap();
        let output = tempfile::tempdir().unwrap();
        let inbox = staging.path().join("inbox");
        fs::create_dir_all(&inbox).unwrap();

        let identity = test_identity();
        let attachment_body = "JVBERi0xLjQK"; // arbitrary base64 payload, byte-identical across both messages

        fs::write(
            inbox.join("1.eml"),
            eml_with_attachment("First", attachment_body),
        )
        .unwrap();
        fs::write(
            inbox.join("2.eml"),
            eml_with_attachment("Second", attachment_body),
        )
        .unwrap();

        let mut message_index =
            ContentIndex::load(staging.path(), ContentIndex::MESSAGE_HASHES).unwrap();
        let mut attachment_index =
            ContentIndex::load(staging.path(), ContentIndex::ATTACHMENT_HASHES).unwrap();

        let first = transform_one(
            &identity,
            &inbox.join("1.eml"),
            staging.path(),
            output.path(),
            &message_index,
            &attachment_index,
        )
        .unwrap()
        .unwrap();
        for (hash, relpath) in &first.pending_attachment_hashes {
            attachment_index
                .commit(staging.path(), hash, relpath)
                .unwrap();
        }
        if let Some((hash, relpath)) = &first.pending_message_hash {
            message_index.commit(staging.path(), hash, relpath).unwrap();
        }

        let second = transform_one(
            &identity,
            &inbox.join("2.eml"),
            staging.path(),
            output.path(),
            &message_index,
            &attachment_index,
        )
        .unwrap()
        .unwrap();

        assert_eq!(second.attachments_deduped, 1);
        assert_eq!(second.attachment_paths, first.attachment_paths);

        let attachments_dir = output
            .path()
            .join(identity::sanitize_segment(&identity.email))
            .join("attachments");
        assert_eq!(fs::read_dir(&attachments_dir).unwrap().count(), 1);
    }

    #[test]
    fn transform_one_merges_byte_identical_message_across_mailboxes() {
        let staging = tempfile::tempdir().unwrap();
        let output = tempfile::tempdir().unwrap();
        let inbox = staging.path().join("inbox");
        let archive = staging.path().join("archive");
        fs::create_dir_all(&inbox).unwrap();
        fs::create_dir_all(&archive).unwrap();

        let identity = test_identity();
        let raw = plain_text_eml("Hello");
        fs::write(inbox.join("1.eml"), &raw).unwrap();
        fs::write(archive.join("2.eml"), &raw).unwrap();

        let mut message_index =
            ContentIndex::load(staging.path(), ContentIndex::MESSAGE_HASHES).unwrap();
        let attachment_index =
            ContentIndex::load(staging.path(), ContentIndex::ATTACHMENT_HASHES).unwrap();

        let first = transform_one(
            &identity,
            &inbox.join("1.eml"),
            staging.path(),
            output.path(),
            &message_index,
            &attachment_index,
        )
        .unwrap()
        .unwrap();
        assert!(!first.merged);
        if let Some((hash, relpath)) = &first.pending_message_hash {
            message_index.commit(staging.path(), hash, relpath).unwrap();
        }

        let second = transform_one(
            &identity,
            &archive.join("2.eml"),
            staging.path(),
            output.path(),
            &message_index,
            &attachment_index,
        )
        .unwrap()
        .unwrap();

        assert!(second.merged);
        assert!(second.canonical_frontmatter_changed);
        assert!(second.attachment_paths.is_empty());
        assert_eq!(second.md_path, first.md_path);

        let contents = fs::read_to_string(&first.md_path).unwrap();
        assert!(contents.contains("mailbox/archive"));
        assert!(contents.contains("also-in:\n  - mailbox/archive#2"));

        // Replaying the exact same duplicate again is idempotent.
        let third = transform_one(
            &identity,
            &archive.join("2.eml"),
            staging.path(),
            output.path(),
            &message_index,
            &attachment_index,
        )
        .unwrap()
        .unwrap();
        assert!(third.merged);
        assert!(!third.canonical_frontmatter_changed);
    }

    fn test_identity() -> Identity {
        Identity {
            alias: "first-last".to_string(),
            email: "first.last@example.com".to_string(),
            provider: crate::email::provider::Provider::Gmail,
            host: "imap.gmail.com".to_string(),
            port: 993,
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

    fn eml_with_attachment(subject: &str, attachment_base64: &str) -> String {
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
             Content-Disposition: attachment; filename=\"a.pdf\"\r\n\
             Content-Transfer-Encoding: base64\r\n\
             \r\n\
             {attachment_base64}\r\n\
             --BOUNDARY--\r\n"
        )
    }
}
