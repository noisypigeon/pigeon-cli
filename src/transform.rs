use std::fs;
use std::path::{Path, PathBuf};

use mail_parser::{Addr, DateTime, MessageParser, MimeHeaders};

use crate::identity::{self, Identity};

/// Summary of a completed `transform` run.
#[derive(Debug, Default)]
pub struct TransformSummary {
    pub messages: usize,
    pub attachments: usize,
    pub skipped: usize,
}

/// Parses every `.eml` file under `input` (as produced by `sink`, per
/// ADR-0005) into a flat, per-identity Markdown tree with YAML frontmatter
/// under `output`, per ADR-0006. Read-only over `input` -- nothing sunk is
/// ever modified or deleted.
pub fn run(identity: &Identity, input: &Path, output: &Path) -> Result<TransformSummary, String> {
    let eml_files = find_eml_files(input)?;

    let identity_dir = output.join(identity::sanitize_segment(&identity.email));
    let attachments_dir = identity_dir.join("attachments");
    fs::create_dir_all(&attachments_dir)
        .map_err(|err| format!("failed to create {}: {err}", attachments_dir.display()))?;

    let mut summary = TransformSummary::default();

    for eml_path in &eml_files {
        let bytes = match fs::read(eml_path) {
            Ok(bytes) => bytes,
            Err(err) => {
                eprintln!("Warning: failed to read {}: {err}", eml_path.display());
                summary.skipped += 1;
                continue;
            }
        };

        let Some(message) = MessageParser::default().parse(&bytes) else {
            eprintln!("Warning: failed to parse {}, skipping", eml_path.display());
            summary.skipped += 1;
            continue;
        };

        let Some(date) = message.date() else {
            eprintln!(
                "Warning: {} has no Date header, skipping",
                eml_path.display()
            );
            summary.skipped += 1;
            continue;
        };

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

        let mut attachment_relpaths = Vec::new();
        for part in message.attachments() {
            let original_name = part.attachment_name().unwrap_or("attachment");
            let attachment_path =
                unique_path(&attachments_dir.join(format!("{stem}-{original_name}")));
            fs::write(&attachment_path, part.contents())
                .map_err(|err| format!("failed to write {}: {err}", attachment_path.display()))?;
            attachment_relpaths.push(format!(
                "attachments/{}",
                attachment_path.file_name().unwrap().to_string_lossy()
            ));
            summary.attachments += 1;
        }

        let mut tags = vec![
            mailbox_tag(eml_path, input),
            format!("identity/{}", identity.alias),
            format!("year/{}", date.year),
        ];
        if let Some(domain_tag) =
            sender_domain_tag(from_addr.and_then(|addr| addr.address.as_deref()))
        {
            tags.push(domain_tag);
        }

        let source = relative_path(
            &fs::canonicalize(&identity_dir)
                .map_err(|err| format!("failed to resolve {}: {err}", identity_dir.display()))?,
            &fs::canonicalize(eml_path)
                .map_err(|err| format!("failed to resolve {}: {err}", eml_path.display()))?,
        );

        let frontmatter = render_frontmatter(
            &format_address(from_addr),
            &format_address(to_addr),
            subject,
            &date.to_rfc3339(),
            &tags,
            &attachment_relpaths,
            &source.to_string_lossy(),
        );

        let md_path = unique_path(&identity_dir.join(format!("{stem}.md")));
        fs::write(&md_path, format!("{frontmatter}\n{body}"))
            .map_err(|err| format!("failed to write {}: {err}", md_path.display()))?;
        summary.messages += 1;
    }

    Ok(summary)
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
    source: &str,
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
    out.push_str(&format!("source: {}\n", yaml_quote(source)));
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

/// A relative path from `from_dir` to `to_path`, both assumed absolute
/// (e.g. from `fs::canonicalize`). Used for the frontmatter `source` field
/// so it stays correct regardless of `--input`/`--output` being relative,
/// absolute, or on unrelated trees.
fn relative_path(from_dir: &Path, to_path: &Path) -> PathBuf {
    let from_components: Vec<_> = from_dir.components().collect();
    let to_components: Vec<_> = to_path.components().collect();

    let common_len = from_components
        .iter()
        .zip(to_components.iter())
        .take_while(|(a, b)| a == b)
        .count();

    let mut result = PathBuf::new();
    for _ in common_len..from_components.len() {
        result.push("..");
    }
    for component in &to_components[common_len..] {
        result.push(component.as_os_str());
    }
    result
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
    fn relative_path_computes_common_ancestor_diff() {
        assert_eq!(
            relative_path(
                Path::new("/tmp/output/jane-doe"),
                Path::new("/tmp/sunk/inbox/1.eml")
            ),
            PathBuf::from("../../sunk/inbox/1.eml")
        );
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
            "../sunk/inbox/482.eml",
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
             source: \"../sunk/inbox/482.eml\"\n\
             ---\n"
        );
    }
}
