use std::collections::HashMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

/// Behavior shared by every content-parsing pipeline this CLI runs.
/// Implemented by `commands::job::email_sync::transform::EmailTransform`
/// (ADR-0023) -- the trait definition lives here since it's a generic
/// shape; the one real implementation lives with its concrete kind.
pub(crate) trait Transform {
    type Input;
    type Output;
    fn transform(&self, input: Self::Input) -> Result<Option<Self::Output>, String>;
}

/// Behavior shared by every content-hash-based deduplication strategy this
/// CLI runs. Implemented by `commands::job::email_sync::dedup::EmailDedup`
/// (ADR-0023), which wraps a `ContentIndex` below -- `ContentIndex` itself
/// stays a plain, trait-free generic utility (ADR-0020), since nothing
/// about *how a hash maps to a path* varies per kind; only what to *do*
/// with a hit/miss (merge a duplicate message vs. reuse a duplicate
/// attachment) varies, which is exactly what these two methods leave to
/// the implementor.
pub(crate) trait Dedup {
    fn check(&self, hash: &str) -> Option<&str>;
    fn commit(&mut self, hash: &str, relative_path: &str) -> Result<(), String>;
}

/// One dotfile's worth of `<hex-md5> <relative-path>` entries -- a durable,
/// append-only content-hash index backing byte-identical-content
/// deduplication (originally ADR-0012, for `pigeon email`'s message/
/// attachment dedup; genericized by ADR-0020 for reuse by other transforms;
/// relocated by ADR-0023). The relative path stored is caller-defined --
/// typically relative to wherever that caller's own transformed output
/// lives.
pub(crate) struct ContentIndex {
    staging_dir: PathBuf,
    file_name: &'static str,
    entries: HashMap<String, String>,
}

impl ContentIndex {
    /// Loads `staging_dir/file_name`. A missing file (first run) is an empty
    /// index. Lines that don't split into `<hash> <path>` are skipped
    /// leniently.
    pub(crate) fn load(
        staging_dir: &Path,
        file_name: &'static str,
    ) -> Result<ContentIndex, String> {
        let path = staging_dir.join(file_name);
        let contents = match fs::read_to_string(&path) {
            Ok(contents) => contents,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(err) => return Err(format!("failed to read {}: {err}", path.display())),
        };
        let entries = contents
            .lines()
            .filter_map(|line| line.split_once(' '))
            .map(|(hash, relpath)| (hash.to_string(), relpath.to_string()))
            .collect();
        Ok(ContentIndex {
            staging_dir: staging_dir.to_path_buf(),
            file_name,
            entries,
        })
    }

    /// Looks up `hash` against every durably committed entry (entries loaded
    /// at start, plus entries `commit`-ted so far this run).
    pub(crate) fn check(&self, hash: &str) -> Option<&str> {
        self.entries.get(hash).map(String::as_str)
    }

    /// Appends one `<hash> <relative_path>` line to `staging_dir/file_name`
    /// (the same `staging_dir` given to `load`) and makes it visible to
    /// every subsequent `check()` this run.
    pub(crate) fn commit(&mut self, hash: &str, relative_path: &str) -> Result<(), String> {
        let path = self.staging_dir.join(self.file_name);
        let mut file = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .map_err(|err| format!("failed to open {}: {err}", path.display()))?;
        writeln!(file, "{hash} {relative_path}")
            .map_err(|err| format!("failed to write {}: {err}", path.display()))?;
        self.entries
            .insert(hash.to_string(), relative_path.to_string());
        Ok(())
    }
}

/// Amends a canonical file's frontmatter for a newly discovered duplicate
/// occurrence: ensures `tag` is present in `tags:`, and inserts (or, keyed
/// by `tag`, updates) an `also-in:` entry recording `occurrence`. A pure
/// textual edit of the existing `---`-delimited block -- no YAML crate.
/// Originally ADR-0012 (`pigeon email`'s mailbox+uid-keyed message
/// merging); genericized by ADR-0020 for reuse by other transforms, where
/// `tag`/`occurrence` can mean whatever that caller's own duplicate-
/// tracking scheme needs them to.
///
/// Returns `Ok(true)` if the file was rewritten (a genuinely new tag/
/// occurrence pair), `Ok(false)` if this exact pair was already recorded
/// (an idempotent resume/crash-recovery replay -- the file is left
/// byte-for-byte untouched). `Err` if `canonical_path` is missing or its
/// frontmatter isn't well-formed; callers treat that as a lenient skip.
pub(crate) fn amend_frontmatter_for_duplicate(
    canonical_path: &Path,
    tag: &str,
    occurrence: u32,
) -> Result<bool, String> {
    let contents = fs::read_to_string(canonical_path)
        .map_err(|err| format!("failed to read {}: {err}", canonical_path.display()))?;
    let had_trailing_newline = contents.ends_with('\n');
    let mut lines: Vec<String> = contents.lines().map(str::to_string).collect();

    if lines.first().map(String::as_str) != Some("---") {
        return Err(format!(
            "{} does not start with a frontmatter delimiter",
            canonical_path.display()
        ));
    }
    let Some(mut close_idx) = lines
        .iter()
        .skip(1)
        .position(|line| line == "---")
        .map(|i| i + 1)
    else {
        return Err(format!(
            "{} has no closing frontmatter delimiter",
            canonical_path.display()
        ));
    };

    let Some(tags_idx) = lines[1..close_idx]
        .iter()
        .position(|line| line == "tags:")
        .map(|i| i + 1)
    else {
        return Err(format!("{} has no tags: field", canonical_path.display()));
    };
    let mut tags_block_end = lines[tags_idx + 1..close_idx]
        .iter()
        .take_while(|line| line.starts_with("  - "))
        .count()
        + tags_idx
        + 1;

    let mut changed = false;
    let tag_line = format!("  - {tag}");
    let has_tag = lines[tags_idx + 1..tags_block_end]
        .iter()
        .any(|line| line == &tag_line);
    if !has_tag {
        lines.insert(tags_block_end, tag_line);
        tags_block_end += 1;
        close_idx += 1;
        changed = true;
    }

    let also_in_header = "also-in:";
    let existing_also_in_idx = lines[tags_block_end..close_idx]
        .iter()
        .position(|line| line == also_in_header)
        .map(|i| i + tags_block_end);

    let new_entry_prefix = format!("  - {tag}#");
    let new_entry = format!("  - {tag}#{occurrence}");

    match existing_also_in_idx {
        Some(also_in_idx) => {
            let also_in_block_end = lines[also_in_idx + 1..close_idx]
                .iter()
                .take_while(|line| line.starts_with("  - "))
                .count()
                + also_in_idx
                + 1;
            let existing_entry_idx = lines[also_in_idx + 1..also_in_block_end]
                .iter()
                .position(|line| line.starts_with(&new_entry_prefix))
                .map(|i| i + also_in_idx + 1);
            match existing_entry_idx {
                Some(idx) => {
                    if lines[idx] != new_entry {
                        lines[idx] = new_entry;
                        changed = true;
                    }
                }
                None => {
                    lines.insert(also_in_block_end, new_entry);
                    changed = true;
                }
            }
        }
        None => {
            lines.insert(tags_block_end, also_in_header.to_string());
            lines.insert(tags_block_end + 1, new_entry);
            changed = true;
        }
    }

    if !changed {
        return Ok(false);
    }

    let mut rewritten = lines.join("\n");
    if had_trailing_newline {
        rewritten.push('\n');
    }
    fs::write(canonical_path, rewritten)
        .map_err(|err| format!("failed to write {}: {err}", canonical_path.display()))?;
    Ok(true)
}

/// Rewrites a single `attachments:` frontmatter entry from `old_relpath` to
/// `new_relpath` -- needed when a message's own `.md` was already written
/// referencing an attachment's staged location, but that attachment turned
/// out to be a cross-message content duplicate (or hit a filename
/// collision) and was placed somewhere else by the post-transform dedup
/// pass (ADR-0021 §7/§10). Same pure-textual-edit technique as
/// `amend_frontmatter_for_duplicate`, targeting an `  - <path>` line instead
/// of the `tags:`/`also-in:` block.
///
/// Returns `Ok(true)` if a line matching `old_relpath` was found and
/// rewritten, `Ok(false)` if it wasn't (already rewritten -- an idempotent
/// resume/crash-recovery replay leaves the file untouched). `Err` if
/// `md_path` can't be read.
pub(crate) fn rewrite_attachment_reference(
    md_path: &Path,
    old_relpath: &str,
    new_relpath: &str,
) -> Result<bool, String> {
    let contents = fs::read_to_string(md_path)
        .map_err(|err| format!("failed to read {}: {err}", md_path.display()))?;
    let had_trailing_newline = contents.ends_with('\n');
    let mut lines: Vec<String> = contents.lines().map(str::to_string).collect();

    let old_line = format!("  - {old_relpath}");
    let Some(idx) = lines.iter().position(|line| line == &old_line) else {
        return Ok(false);
    };
    lines[idx] = format!("  - {new_relpath}");

    let mut rewritten = lines.join("\n");
    if had_trailing_newline {
        rewritten.push('\n');
    }
    fs::write(md_path, rewritten)
        .map_err(|err| format!("failed to write {}: {err}", md_path.display()))?;
    Ok(true)
}

/// Escapes `s` as a double-quoted YAML scalar. Beyond `\`/`"`, also escapes
/// every C0 control character (including a raw newline or carriage return)
/// via YAML's own double-quoted escape syntax, rather than stripping them --
/// an unescaped control character wouldn't violate YAML's own scalar rules,
/// but this codebase's frontmatter is re-parsed as flat `\n`-split lines by
/// `amend_frontmatter_for_duplicate`, so a smuggled newline could inject a
/// fake `tags:`/`---` line and desync its rewriter.
pub(crate) fn yaml_quote(s: &str) -> String {
    let mut escaped = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\\' => escaped.push_str("\\\\"),
            '"' => escaped.push_str("\\\""),
            '\n' => escaped.push_str("\\n"),
            '\r' => escaped.push_str("\\r"),
            '\t' => escaped.push_str("\\t"),
            c if (c as u32) < 0x20 || c as u32 == 0x7f => {
                escaped.push_str(&format!("\\x{:02x}", c as u32));
            }
            c => escaped.push(c),
        }
    }
    format!("\"{escaped}\"")
}

/// If `desired` doesn't exist yet, returns it as-is; otherwise appends
/// `-2`, `-3`, ... before the extension until a free path is found.
pub(crate) fn unique_path(desired: &Path) -> PathBuf {
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

/// Caps a sanitized filename's length so it can never blow past a
/// filesystem's per-component name limit (255 bytes on APFS/most Unix
/// filesystems) once stacked onto whatever prefix a caller appends it to.
const MAX_FILENAME_LENGTH: usize = 100;

/// Reduces an untrusted, externally-sourced name to a safe filename: keeps
/// only the final path component (so an embedded `/` can't make
/// `Path::join` create an implicit, never-created subdirectory, per
/// ADR-0013), caps its length (an externally-sourced name can be
/// arbitrarily long), and falls back to `"file"` if nothing usable remains.
/// Extension is preserved where reasonable, unlike
/// `email::identity::sanitize_segment`, which would corrupt it. Originally
/// ADR-0013's MIME-attachment-name fix; genericized by ADR-0020.
pub(crate) fn sanitize_filename(name: &str) -> String {
    let base = match Path::new(name).file_name().and_then(|f| f.to_str()) {
        Some(base) if !base.is_empty() => base,
        _ => return "file".to_string(),
    };
    truncate_preserving_extension(base, MAX_FILENAME_LENGTH)
}

/// Truncates `name` to at most `max_len` bytes. If it has a short-enough
/// extension (text after the last `.`), the stem is truncated and the
/// extension kept intact rather than risking cutting it off mid-string.
/// Always cuts on a UTF-8 char boundary (a sanitized name, unlike
/// `identity::sanitize_segment`'s output, isn't restricted to ASCII).
fn truncate_preserving_extension(name: &str, max_len: usize) -> String {
    if name.len() <= max_len {
        return name.to_string();
    }
    if let Some((stem, ext)) = name.rsplit_once('.')
        && !ext.is_empty()
        && ext.len() + 1 < max_len
    {
        return format!(
            "{}.{ext}",
            truncate_at_char_boundary(stem, max_len - ext.len() - 1)
        );
    }
    truncate_at_char_boundary(name, max_len)
}

fn truncate_at_char_boundary(s: &str, max_bytes: usize) -> String {
    let mut end = max_bytes.min(s.len());
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    s[..end].to_string()
}

/// Recursively collects every file under `dir`, sorted for deterministic
/// order. A missing `dir` is treated as an empty result, not an error --
/// callers that need "does this path exist at all" semantics (e.g. a single
/// file vs. directory vs. missing distinction) check that themselves before
/// calling this.
pub(crate) fn collect_files(dir: &Path) -> Result<Vec<PathBuf>, String> {
    let mut files = Vec::new();
    if dir.exists() {
        visit_dir(dir, &mut files)?;
    }
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
        } else {
            files.push(path);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const MESSAGE_HASHES: &str = ".message-hashes";
    const ATTACHMENT_HASHES: &str = ".attachment-hashes";

    const FIXTURE: &str = "---\n\
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
        ---\n\
        \n\
        Hello there!\n";

    #[test]
    fn load_missing_file_is_empty() {
        let dir = tempfile::tempdir().unwrap();
        let index = ContentIndex::load(dir.path(), MESSAGE_HASHES).unwrap();
        assert!(index.check("abc").is_none());
    }

    #[test]
    fn commit_then_check_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let mut index = ContentIndex::load(dir.path(), ATTACHMENT_HASHES).unwrap();
        index.commit("hash1", "identity/attachments/a.pdf").unwrap();
        assert_eq!(index.check("hash1"), Some("identity/attachments/a.pdf"));
        assert!(index.check("hash2").is_none());
    }

    #[test]
    fn load_picks_up_entries_committed_by_a_prior_load() {
        let dir = tempfile::tempdir().unwrap();
        let mut first = ContentIndex::load(dir.path(), MESSAGE_HASHES).unwrap();
        first.commit("hash1", "identity/a.md").unwrap();

        let second = ContentIndex::load(dir.path(), MESSAGE_HASHES).unwrap();
        assert_eq!(second.check("hash1"), Some("identity/a.md"));
    }

    #[test]
    fn load_skips_malformed_lines() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join(MESSAGE_HASHES), "no-space-here\n").unwrap();
        let index = ContentIndex::load(dir.path(), MESSAGE_HASHES).unwrap();
        assert!(index.check("no-space-here").is_none());
    }

    #[test]
    fn amend_adds_new_tag_and_also_in_entry() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("canonical.md");
        fs::write(&path, FIXTURE).unwrap();

        let changed = amend_frontmatter_for_duplicate(&path, "mailbox/archive", 45).unwrap();
        assert!(changed);

        let contents = fs::read_to_string(&path).unwrap();
        assert!(contents.contains("  - mailbox/archive\n"));
        assert!(contents.contains("also-in:\n  - mailbox/archive#45\n"));
        assert!(contents.contains("from: \"Jane Doe <jane.doe@example.com>\""));
        assert!(
            contents.contains("attachments:\n  - attachments/2024-01-26-hello-world-bingo.pdf")
        );
        assert!(contents.contains("uid: 482"));
        assert!(contents.ends_with("Hello there!\n"));
    }

    #[test]
    fn amend_is_idempotent_for_identical_tag_and_occurrence() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("canonical.md");
        fs::write(&path, FIXTURE).unwrap();

        amend_frontmatter_for_duplicate(&path, "mailbox/archive", 45).unwrap();
        let after_first = fs::read_to_string(&path).unwrap();

        let changed = amend_frontmatter_for_duplicate(&path, "mailbox/archive", 45).unwrap();
        assert!(!changed);
        assert_eq!(fs::read_to_string(&path).unwrap(), after_first);
    }

    #[test]
    fn amend_updates_occurrence_in_place() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("canonical.md");
        fs::write(&path, FIXTURE).unwrap();

        amend_frontmatter_for_duplicate(&path, "mailbox/archive", 45).unwrap();
        let changed = amend_frontmatter_for_duplicate(&path, "mailbox/archive", 99).unwrap();
        assert!(changed);

        let contents = fs::read_to_string(&path).unwrap();
        assert!(contents.contains("  - mailbox/archive#99"));
        assert!(!contents.contains("  - mailbox/archive#45"));
        assert_eq!(contents.matches("mailbox/archive#").count(), 1);
    }

    #[test]
    fn amend_errors_on_missing_canonical_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("does-not-exist.md");
        assert!(amend_frontmatter_for_duplicate(&path, "mailbox/archive", 45).is_err());
    }

    #[test]
    fn rewrite_attachment_reference_replaces_matching_line() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("canonical.md");
        fs::write(&path, FIXTURE).unwrap();

        let changed = rewrite_attachment_reference(
            &path,
            "attachments/2024-01-26-hello-world-bingo.pdf",
            "attachments/2024-01-26-hello-world-bingo-2.pdf",
        )
        .unwrap();
        assert!(changed);

        let contents = fs::read_to_string(&path).unwrap();
        assert!(
            contents.contains("attachments:\n  - attachments/2024-01-26-hello-world-bingo-2.pdf")
        );
        assert!(!contents.contains("attachments/2024-01-26-hello-world-bingo.pdf\n"));
    }

    #[test]
    fn rewrite_attachment_reference_is_idempotent_when_old_path_already_gone() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("canonical.md");
        fs::write(&path, FIXTURE).unwrap();

        rewrite_attachment_reference(
            &path,
            "attachments/2024-01-26-hello-world-bingo.pdf",
            "attachments/canonical.pdf",
        )
        .unwrap();
        let after_first = fs::read_to_string(&path).unwrap();

        let changed = rewrite_attachment_reference(
            &path,
            "attachments/2024-01-26-hello-world-bingo.pdf",
            "attachments/canonical.pdf",
        )
        .unwrap();
        assert!(!changed);
        assert_eq!(fs::read_to_string(&path).unwrap(), after_first);
    }

    #[test]
    fn rewrite_attachment_reference_errors_on_missing_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("does-not-exist.md");
        assert!(
            rewrite_attachment_reference(&path, "attachments/a.pdf", "attachments/b.pdf").is_err()
        );
    }

    #[test]
    fn commit_survives_concurrent_access_from_multiple_threads() {
        use std::sync::{Arc, Mutex};
        use std::thread;

        let dir = tempfile::tempdir().unwrap();
        let index = Arc::new(Mutex::new(
            ContentIndex::load(dir.path(), ATTACHMENT_HASHES).unwrap(),
        ));

        let handles: Vec<_> = (0..8)
            .map(|n| {
                let index = Arc::clone(&index);
                thread::spawn(move || {
                    index
                        .lock()
                        .unwrap()
                        .commit(&format!("hash{n}"), &format!("identity/a{n}.pdf"))
                        .unwrap();
                })
            })
            .collect();
        for handle in handles {
            handle.join().unwrap();
        }

        let guard = index.lock().unwrap();
        for n in 0..8 {
            assert_eq!(
                guard.check(&format!("hash{n}")),
                Some(format!("identity/a{n}.pdf")).as_deref()
            );
        }

        let contents = fs::read_to_string(dir.path().join(ATTACHMENT_HASHES)).unwrap();
        assert_eq!(contents.lines().count(), 8);
    }

    #[test]
    fn yaml_quote_escapes_quotes_and_backslashes() {
        assert_eq!(yaml_quote("Hello: World"), "\"Hello: World\"");
        assert_eq!(yaml_quote(r#"She said "hi""#), r#""She said \"hi\"""#);
    }

    #[test]
    fn yaml_quote_escapes_embedded_newline_and_carriage_return() {
        assert_eq!(yaml_quote("line1\nline2"), "\"line1\\nline2\"");
        assert_eq!(yaml_quote("a\r\nb"), "\"a\\r\\nb\"");
    }

    #[test]
    fn yaml_quote_escapes_other_control_characters() {
        assert_eq!(yaml_quote("a\tb"), "\"a\\tb\"");
        assert_eq!(yaml_quote("a\x01b"), "\"a\\x01b\"");
    }

    #[test]
    fn yaml_quote_leaves_non_ascii_names_unchanged() {
        assert_eq!(yaml_quote("José García"), "\"José García\"");
    }

    #[test]
    fn yaml_quote_prevents_frontmatter_line_injection() {
        // The concrete attack this fix closes: a sender display name
        // carrying a raw newline followed by a fake `tags:` line must come
        // back as a single escaped scalar, not a value that reintroduces a
        // literal newline once written to the frontmatter.
        let malicious = "Evil\ntags:\n  - admin";
        assert!(!yaml_quote(malicious).contains('\n'));
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
    fn sanitize_filename_strips_leading_slash() {
        assert_eq!(sanitize_filename("/img0.png"), "img0.png");
    }

    #[test]
    fn sanitize_filename_strips_nested_directories() {
        assert_eq!(sanitize_filename("a/b/c.pdf"), "c.pdf");
    }

    #[test]
    fn sanitize_filename_preserves_normal_name() {
        assert_eq!(sanitize_filename("report.pdf"), "report.pdf");
    }

    #[test]
    fn sanitize_filename_falls_back_for_dot_dot() {
        assert_eq!(sanitize_filename(".."), "file");
    }

    #[test]
    fn sanitize_filename_falls_back_for_bare_slash() {
        assert_eq!(sanitize_filename("/"), "file");
    }

    #[test]
    fn sanitize_filename_truncates_long_name_preserving_extension() {
        let long_name = format!("{}.pdf", "a".repeat(300));
        let sanitized = sanitize_filename(&long_name);
        assert!(sanitized.len() <= MAX_FILENAME_LENGTH);
        assert!(sanitized.ends_with(".pdf"));
    }

    #[test]
    fn sanitize_filename_truncates_long_name_with_no_extension() {
        let long_name = "a".repeat(300);
        let sanitized = sanitize_filename(&long_name);
        assert!(sanitized.len() <= MAX_FILENAME_LENGTH);
    }

    #[test]
    fn sanitize_filename_truncates_multibyte_name_at_char_boundary() {
        // Each "é" is 2 bytes in UTF-8; a naive byte-count truncation could
        // split one in half and panic.
        let long_name = format!("{}.png", "é".repeat(200));
        let sanitized = sanitize_filename(&long_name);
        assert!(sanitized.len() <= MAX_FILENAME_LENGTH);
        assert!(sanitized.ends_with(".png"));
        assert!(sanitized.is_char_boundary(sanitized.len()));
    }

    #[test]
    fn collect_files_walks_nested_directories() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("attachments")).unwrap();
        fs::write(dir.path().join("hello.md"), b"hi").unwrap();
        fs::write(dir.path().join("attachments/a.pdf"), b"pdf").unwrap();

        let mut files = collect_files(dir.path()).unwrap();
        files.sort();

        let mut expected = vec![
            dir.path().join("attachments/a.pdf"),
            dir.path().join("hello.md"),
        ];
        expected.sort();

        assert_eq!(files, expected);
    }

    #[test]
    fn collect_files_missing_directory_is_empty() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("does-not-exist");
        assert!(collect_files(&missing).unwrap().is_empty());
    }
}
