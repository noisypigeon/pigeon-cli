use std::collections::HashMap;
use std::fs;
use std::io::Write;
use std::path::Path;

/// One dotfile's worth of `<hex-md5> <relative-path>` entries -- the durable,
/// cross-mailbox record backing ADR-0012's content dedup. The relative path
/// stored is relative to the identity's own output directory (the same
/// convention `attachments:` frontmatter entries already use), not
/// `staging_dir` (where the index file itself lives).
pub(crate) struct ContentIndex {
    file_name: &'static str,
    entries: HashMap<String, String>,
}

impl ContentIndex {
    pub(crate) const ATTACHMENT_HASHES: &'static str = ".attachment-hashes";
    pub(crate) const MESSAGE_HASHES: &'static str = ".message-hashes";

    /// Loads `staging_dir/file_name`. A missing file (first run) is an empty
    /// index, matching `sync::read_processed`'s convention. Lines that don't
    /// split into `<hash> <path>` are skipped leniently.
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
        Ok(ContentIndex { file_name, entries })
    }

    /// Looks up `hash` against every durably committed entry (entries loaded
    /// at start, plus entries `commit`-ted so far this run).
    pub(crate) fn check(&self, hash: &str) -> Option<&str> {
        self.entries.get(hash).map(String::as_str)
    }

    /// Appends one `<hash> <relative_path>` line to `staging_dir/file_name`
    /// and makes it visible to every subsequent `check()` this run.
    pub(crate) fn commit(
        &mut self,
        staging_dir: &Path,
        hash: &str,
        relative_path: &str,
    ) -> Result<(), String> {
        let path = staging_dir.join(self.file_name);
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

/// Amends a canonical `.md`'s frontmatter for a newly discovered duplicate
/// occurrence: ensures `mailbox_tag` is present in `tags:`, and inserts (or,
/// keyed by mailbox, updates) an `also-in:` entry recording `uid`. A pure
/// textual edit of the existing `---`-delimited block, matching
/// `transform::render_frontmatter`'s hand-rolled style -- no YAML crate.
///
/// Returns `Ok(true)` if the file was rewritten (a genuinely new mailbox/uid
/// pair), `Ok(false)` if this exact mailbox/uid pair was already recorded
/// (an idempotent resume/crash-recovery replay -- the file is left
/// byte-for-byte untouched). `Err` if `canonical_md_path` is missing or its
/// frontmatter isn't well-formed; callers treat that as a lenient skip.
pub(crate) fn amend_frontmatter_for_duplicate(
    canonical_md_path: &Path,
    mailbox_tag: &str,
    uid: u32,
) -> Result<bool, String> {
    let contents = fs::read_to_string(canonical_md_path)
        .map_err(|err| format!("failed to read {}: {err}", canonical_md_path.display()))?;
    let had_trailing_newline = contents.ends_with('\n');
    let mut lines: Vec<String> = contents.lines().map(str::to_string).collect();

    if lines.first().map(String::as_str) != Some("---") {
        return Err(format!(
            "{} does not start with a frontmatter delimiter",
            canonical_md_path.display()
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
            canonical_md_path.display()
        ));
    };

    let Some(tags_idx) = lines[1..close_idx]
        .iter()
        .position(|line| line == "tags:")
        .map(|i| i + 1)
    else {
        return Err(format!(
            "{} has no tags: field",
            canonical_md_path.display()
        ));
    };
    let mut tags_block_end = lines[tags_idx + 1..close_idx]
        .iter()
        .take_while(|line| line.starts_with("  - "))
        .count()
        + tags_idx
        + 1;

    let mut changed = false;
    let mailbox_line = format!("  - {mailbox_tag}");
    let has_mailbox_tag = lines[tags_idx + 1..tags_block_end]
        .iter()
        .any(|line| line == &mailbox_line);
    if !has_mailbox_tag {
        lines.insert(tags_block_end, mailbox_line);
        tags_block_end += 1;
        close_idx += 1;
        changed = true;
    }

    let also_in_header = "also-in:";
    let existing_also_in_idx = lines[tags_block_end..close_idx]
        .iter()
        .position(|line| line == also_in_header)
        .map(|i| i + tags_block_end);

    let new_entry_prefix = format!("  - {mailbox_tag}#");
    let new_entry = format!("  - {mailbox_tag}#{uid}");

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
    fs::write(canonical_md_path, rewritten)
        .map_err(|err| format!("failed to write {}: {err}", canonical_md_path.display()))?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let index = ContentIndex::load(dir.path(), ContentIndex::MESSAGE_HASHES).unwrap();
        assert!(index.check("abc").is_none());
    }

    #[test]
    fn commit_then_check_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let mut index = ContentIndex::load(dir.path(), ContentIndex::ATTACHMENT_HASHES).unwrap();
        index
            .commit(dir.path(), "hash1", "identity/attachments/a.pdf")
            .unwrap();
        assert_eq!(index.check("hash1"), Some("identity/attachments/a.pdf"));
        assert!(index.check("hash2").is_none());
    }

    #[test]
    fn load_picks_up_entries_committed_by_a_prior_load() {
        let dir = tempfile::tempdir().unwrap();
        let mut first = ContentIndex::load(dir.path(), ContentIndex::MESSAGE_HASHES).unwrap();
        first.commit(dir.path(), "hash1", "identity/a.md").unwrap();

        let second = ContentIndex::load(dir.path(), ContentIndex::MESSAGE_HASHES).unwrap();
        assert_eq!(second.check("hash1"), Some("identity/a.md"));
    }

    #[test]
    fn load_skips_malformed_lines() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join(ContentIndex::MESSAGE_HASHES),
            "no-space-here\n",
        )
        .unwrap();
        let index = ContentIndex::load(dir.path(), ContentIndex::MESSAGE_HASHES).unwrap();
        assert!(index.check("no-space-here").is_none());
    }

    #[test]
    fn amend_adds_new_mailbox_tag_and_also_in_entry() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("canonical.md");
        fs::write(&path, FIXTURE).unwrap();

        let changed = amend_frontmatter_for_duplicate(&path, "mailbox/archive", 45).unwrap();
        assert!(changed);

        let contents = fs::read_to_string(&path).unwrap();
        assert!(contents.contains("  - mailbox/archive\n"));
        assert!(contents.contains("also-in:\n  - mailbox/archive#45\n"));
        // Untouched fields.
        assert!(contents.contains("from: \"Jane Doe <jane.doe@example.com>\""));
        assert!(
            contents.contains("attachments:\n  - attachments/2024-01-26-hello-world-bingo.pdf")
        );
        assert!(contents.contains("uid: 482"));
        assert!(contents.ends_with("Hello there!\n"));
    }

    #[test]
    fn amend_is_idempotent_for_identical_mailbox_and_uid() {
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
    fn amend_updates_uid_in_place_on_uidvalidity_reset() {
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
}
