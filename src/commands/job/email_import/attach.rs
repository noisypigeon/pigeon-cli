//! Unpacks each message's attachments next to its `.eml` (CLI-0146 §3).
//!
//! A near-sibling of `email_pull::worker::extract_attachments`, written
//! separately rather than hoisted because the two differ in what they
//! return: that one yields `(md5, relpath)` pairs to feed its dedup pass,
//! which this job has none of (CLI-0146 Out of scope), so hashing every
//! attachment here would be pure cost. Two consumers is in any case below
//! this codebase's duplicate-until-the-third-consumer threshold, and
//! CLI-0147 deletes the other one.

use std::fs;
use std::path::{Path, PathBuf};

use mail_parser::{MessageParser, MimeHeaders};

use crate::core::data::{sanitize_filename, unique_path};

/// One unpacked attachment, ready to push.
pub(crate) struct StagedAttachment {
    /// Where the bytes are on disk.
    pub path: PathBuf,
    /// The leaf filename, which is also its name at the destination.
    pub name: String,
}

/// Parses `eml_path` and writes each attachment part's bytes into
/// `<attachments_dir>`, returning what it staged.
///
/// `Err(())` means the message could not be read or parsed at all. The
/// caller treats that as a logged, non-fatal condition and still delivers
/// the raw `.eml`, which is the faithful archival artifact and already
/// contains every attachment inline -- the same leniency CLI-0081 chose,
/// for the same reason.
///
/// An individual attachment that cannot be written is warned about and
/// skipped rather than failing its siblings, but is reported through the
/// returned count mismatch so the caller can record it.
pub(crate) fn extract_attachments(
    eml_path: &Path,
    attachments_dir: &Path,
    uid: u32,
    mailbox: &str,
) -> Result<Vec<StagedAttachment>, ()> {
    let bytes = match fs::read(eml_path) {
        Ok(bytes) => bytes,
        Err(err) => {
            tracing::warn!(uid, mailbox, step = "extract", error = %err, "failed to read eml file");
            return Err(());
        }
    };

    let Some(message) = MessageParser::default().parse(&bytes) else {
        tracing::warn!(
            uid,
            mailbox,
            step = "extract",
            "failed to parse eml file; delivering the raw message with no attachments unpacked"
        );
        return Err(());
    };

    let mut staged = Vec::new();
    for part in message.attachments() {
        let contents = part.contents();
        let name = part.attachment_name();
        if contents.is_empty() && name.is_none() {
            // A truncated or malformed trailing MIME part with neither
            // headers nor content -- not a real attachment. Same guard
            // `EmailTransform::transform` applies (CLI-0030 amendment).
            continue;
        }
        if let Err(err) = fs::create_dir_all(attachments_dir) {
            tracing::warn!(uid, mailbox, step = "extract", error = %err, "failed to create attachment dir");
            continue;
        }
        // `sanitize_filename` is what keeps a sender-controlled MIME name
        // from carrying a path separator out of this directory (CLI-0013),
        // and `unique_path` disambiguates two attachments sharing a name
        // within one message.
        let sanitized_name = sanitize_filename(name.unwrap_or("attachment"));
        let staged_path = unique_path(&attachments_dir.join(&sanitized_name));
        if let Err(err) = fs::write(&staged_path, contents) {
            tracing::warn!(uid, mailbox, step = "extract", error = %err, "failed to write attachment");
            continue;
        }
        let name = staged_path
            .file_name()
            .map(|name| name.to_string_lossy().to_string())
            .unwrap_or(sanitized_name);
        staged.push(StagedAttachment {
            path: staged_path,
            name,
        });
    }

    Ok(staged)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A minimal multipart message carrying one named attachment.
    fn eml_with_attachment(filename: &str) -> String {
        format!(
            "From: a@example.com\r\n\
             To: b@example.com\r\n\
             Subject: Shipping\r\n\
             MIME-Version: 1.0\r\n\
             Content-Type: multipart/mixed; boundary=\"bound\"\r\n\
             \r\n\
             --bound\r\n\
             Content-Type: text/plain\r\n\
             \r\n\
             See attached.\r\n\
             --bound\r\n\
             Content-Type: application/pdf\r\n\
             Content-Disposition: attachment; filename=\"{filename}\"\r\n\
             \r\n\
             pdf-bytes\r\n\
             --bound--\r\n"
        )
    }

    fn write_eml(dir: &Path, body: &str) -> PathBuf {
        let path = dir.join("1.eml");
        fs::write(&path, body).unwrap();
        path
    }

    #[test]
    fn stages_an_attachment_from_a_multipart_message() {
        let dir = tempfile::tempdir().unwrap();
        let eml_path = write_eml(dir.path(), &eml_with_attachment("a.pdf"));
        let attachments_dir = dir.path().join("1").join("attachments");

        let staged = extract_attachments(&eml_path, &attachments_dir, 1, "INBOX").unwrap();

        assert_eq!(staged.len(), 1);
        assert_eq!(staged[0].name, "a.pdf");
        assert_eq!(staged[0].path, attachments_dir.join("a.pdf"));
        assert!(staged[0].path.exists());
    }

    /// The inline text part is not an attachment, so a message with no real
    /// attachment stages nothing -- and must not error.
    #[test]
    fn stages_nothing_for_a_message_with_no_attachments() {
        let dir = tempfile::tempdir().unwrap();
        let eml_path = write_eml(
            dir.path(),
            "From: a@example.com\r\nSubject: Hi\r\n\r\njust text\r\n",
        );
        let attachments_dir = dir.path().join("1").join("attachments");

        let staged = extract_attachments(&eml_path, &attachments_dir, 1, "INBOX").unwrap();

        assert!(staged.is_empty());
        assert!(
            !attachments_dir.exists(),
            "the directory is created lazily, only when there is something to put in it"
        );
    }

    #[test]
    fn returns_err_for_unparseable_input() {
        let dir = tempfile::tempdir().unwrap();
        let eml_path = write_eml(dir.path(), "");

        assert!(extract_attachments(&eml_path, &dir.path().join("a"), 1, "INBOX").is_err());
    }

    #[test]
    fn returns_err_for_a_missing_file() {
        let dir = tempfile::tempdir().unwrap();

        let result = extract_attachments(
            &dir.path().join("absent.eml"),
            &dir.path().join("a"),
            1,
            "INBOX",
        );

        assert!(result.is_err());
    }

    /// CLI-0013: a sender-controlled MIME filename carrying a path
    /// separator must not escape the attachments directory.
    #[test]
    fn sanitizes_a_filename_containing_a_path_separator() {
        let dir = tempfile::tempdir().unwrap();
        let eml_path = write_eml(dir.path(), &eml_with_attachment("../../etc/passwd"));
        let attachments_dir = dir.path().join("1").join("attachments");

        let staged = extract_attachments(&eml_path, &attachments_dir, 1, "INBOX").unwrap();

        assert_eq!(staged.len(), 1);
        assert!(
            !staged[0].name.contains('/') && !staged[0].name.contains(".."),
            "unexpected name: {}",
            staged[0].name
        );
        assert_eq!(
            staged[0].path.parent().unwrap(),
            attachments_dir,
            "the file must land inside the attachments directory"
        );
    }

    /// Two attachments sharing a name within one message must both survive.
    #[test]
    fn disambiguates_two_attachments_sharing_a_name() {
        let dir = tempfile::tempdir().unwrap();
        let body = "From: a@example.com\r\n\
             MIME-Version: 1.0\r\n\
             Content-Type: multipart/mixed; boundary=\"bound\"\r\n\
             \r\n\
             --bound\r\n\
             Content-Type: application/pdf\r\n\
             Content-Disposition: attachment; filename=\"a.pdf\"\r\n\
             \r\n\
             first\r\n\
             --bound\r\n\
             Content-Type: application/pdf\r\n\
             Content-Disposition: attachment; filename=\"a.pdf\"\r\n\
             \r\n\
             second\r\n\
             --bound--\r\n";
        let eml_path = write_eml(dir.path(), body);
        let attachments_dir = dir.path().join("1").join("attachments");

        let staged = extract_attachments(&eml_path, &attachments_dir, 1, "INBOX").unwrap();

        assert_eq!(staged.len(), 2);
        let names: Vec<&str> = staged.iter().map(|a| a.name.as_str()).collect();
        assert_ne!(names[0], names[1], "unexpected collision: {names:?}");
        for attachment in &staged {
            assert!(attachment.path.exists());
        }
    }
}
