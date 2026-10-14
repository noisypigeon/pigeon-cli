use std::collections::HashMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
/// One hash's committed entry: the final relative path, plus (ADR-0099) the
/// original source key of whichever file was kept under that path --
/// empty when the entry predates ADR-0099 or was committed by a caller that
/// only ever uses the plain `Dedup::commit` (original key is never
/// retroactively backfilled, since a hash is only ever committed once).
/// `source_bucket_alias` (ADR-0109) is likewise empty unless committed via
/// `commit_with_key_and_bucket` -- only `deduplicate::dedup` does that
/// today, to record which of its (possibly several) source buckets a kept
/// file came from.
#[derive(Clone)]
struct ContentIndexEntry {
    relative_path: String,
    original_key: String,
    source_bucket_alias: String,
}

/// One dotfile's worth of `<hex-md5>\t<relative-path>\t<original-key>`
/// entries (optionally a 4th `\t<source-bucket-alias>` field, ADR-0109) --
/// a durable, append-only content-hash index backing byte-identical-content
/// deduplication (originally ADR-0012, for `pigeon email`'s message/
/// attachment dedup; genericized by ADR-0020 for reuse by other
/// transforms; relocated by ADR-0023; gained the tab-separated
/// `original-key` field in ADR-0099). The relative path stored is
/// caller-defined -- typically relative to wherever that caller's own
/// transformed output lives.
pub(crate) struct ContentIndex {
    staging_dir: PathBuf,
    file_name: &'static str,
    entries: HashMap<String, ContentIndexEntry>,
}

impl ContentIndex {
    /// Loads `staging_dir/file_name`. A missing file (first run) is an empty
    /// index. A line containing a tab is read as the current
    /// `<hash>\t<relative_path>\t<original_key>\t<source_bucket_alias>`
    /// format (the trailing `original_key`/`source_bucket_alias` fields are
    /// each optional, for forward compatibility with older 2-/3-field
    /// lines); a line with no tab falls back to the pre-ADR-0099
    /// `<hash> <relative_path>` format, with both trailing fields
    /// defaulting to `""`. Lines matching neither shape are skipped
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
            .filter_map(|line| {
                if let Some((hash, rest)) = line.split_once('\t') {
                    let (relative_path, rest) = rest.split_once('\t').unwrap_or((rest, ""));
                    let (original_key, source_bucket_alias) =
                        rest.split_once('\t').unwrap_or((rest, ""));
                    Some((
                        hash.to_string(),
                        ContentIndexEntry {
                            relative_path: relative_path.to_string(),
                            original_key: original_key.to_string(),
                            source_bucket_alias: source_bucket_alias.to_string(),
                        },
                    ))
                } else {
                    line.split_once(' ').map(|(hash, relative_path)| {
                        (
                            hash.to_string(),
                            ContentIndexEntry {
                                relative_path: relative_path.to_string(),
                                original_key: String::new(),
                                source_bucket_alias: String::new(),
                            },
                        )
                    })
                }
            })
            .collect();
        Ok(ContentIndex {
            staging_dir: staging_dir.to_path_buf(),
            file_name,
            entries,
        })
    }
    /// Like `check`, but also returns the original source key of the kept
    /// file (ADR-0099) and the source bucket alias it was downloaded from
    /// (ADR-0109) -- both `""` when the entry predates the respective ADR
    /// or was committed via the plain `commit`/`commit_with_key`/
    /// `Dedup::commit`. Only `deduplicate::dedup` calls this today; every
    /// other `ContentIndex` consumer has no multi-bucket concept and
    /// doesn't need the extra field.
    pub(crate) fn check_with_original_key_and_bucket(
        &self,
        hash: &str,
    ) -> Option<(&str, &str, &str)> {
        self.entries.get(hash).map(|entry| {
            (
                entry.relative_path.as_str(),
                entry.original_key.as_str(),
                entry.source_bucket_alias.as_str(),
            )
        })
    }
    /// Like `commit_with_key`, but also durably records which source
    /// bucket-config `original_key` was downloaded from (ADR-0109), as a
    /// 4th tab-separated field -- so a later duplicate's report row can
    /// name not just which key was kept, but which of this run's (possibly
    /// several) source buckets it came from. Only `deduplicate::dedup`
    /// calls this, and since ADR-0147 removed the two email jobs it is the
    /// only consumer left -- so every line written today carries all four
    /// fields. The 2- and 3-field forms are still read (see `load`), since
    /// an index written by an older build is still valid on disk.
    pub(crate) fn commit_with_key_and_bucket(
        &mut self,
        hash: &str,
        relative_path: &str,
        original_key: &str,
        source_bucket_alias: &str,
    ) -> Result<(), String> {
        let path = self.staging_dir.join(self.file_name);
        let mut file = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .map_err(|err| format!("failed to open {}: {err}", path.display()))?;
        writeln!(
            file,
            "{hash}\t{relative_path}\t{original_key}\t{source_bucket_alias}"
        )
        .map_err(|err| format!("failed to write {}: {err}", path.display()))?;
        self.entries.insert(
            hash.to_string(),
            ContentIndexEntry {
                relative_path: relative_path.to_string(),
                original_key: original_key.to_string(),
                source_bucket_alias: source_bucket_alias.to_string(),
            },
        );
        Ok(())
    }
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

/// `true` when `key`'s final `/`-separated segment is a macOS AppleDouble
/// resource-fork sidecar name (`._<name>`). Usable on a zip-internal entry
/// name (via `archive::is_apple_metadata_entry`, which additionally treats a
/// `__MACOSX/` path prefix as metadata, a convention with no equivalent on a
/// plain bucket key), directly on a top-level bucket object's own display key
/// (ADR-0107), or on a source-tree relative path (`transform`, ADR-0130 --
/// these 82-byte stubs match an `--include '*.<ext>'` filter but are not
/// decodable media, and feeding one to `ffmpeg` produces a non-retryable
/// error that can trip the circuit breaker). Hoisted here on its second
/// consumer (ADR-0130), alongside this module's other filename predicates;
/// `commands::job::archive` is the other caller.
pub(crate) fn is_apple_double_basename(key: &str) -> bool {
    key.rsplit('/').next().unwrap_or(key).starts_with("._")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_apple_double_basename_matches_only_a_resource_fork_sidecar() {
        assert!(is_apple_double_basename("._IMG_0001.heic"));
        assert!(is_apple_double_basename("photos/nested/._IMG_0001.heic"));
        assert!(!is_apple_double_basename("IMG_0001.heic"));
        assert!(!is_apple_double_basename("photos/IMG_0001.heic"));
        // A leading dot alone is an ordinary hidden file, not AppleDouble.
        assert!(!is_apple_double_basename(".hidden.heic"));
        // The `._` must be in the final segment, not an ancestor directory.
        assert!(!is_apple_double_basename("._dir/IMG_0001.heic"));
    }

    #[test]
    fn content_index_commit_with_key_and_bucket_round_trips_the_source_bucket_alias() {
        let dir = tempfile::tempdir().unwrap();
        let mut index = ContentIndex::load(dir.path(), ".content-hashes").unwrap();

        index
            .commit_with_key_and_bucket("hash-a", "pdf/report.pdf", "docs/report.pdf", "bucket-a")
            .unwrap();

        assert_eq!(
            index.check_with_original_key_and_bucket("hash-a"),
            Some(("pdf/report.pdf", "docs/report.pdf", "bucket-a"))
        );

        // Reload from disk to confirm the 4-field line persisted correctly.
        let reloaded = ContentIndex::load(dir.path(), ".content-hashes").unwrap();
        assert_eq!(
            reloaded.check_with_original_key_and_bucket("hash-a"),
            Some(("pdf/report.pdf", "docs/report.pdf", "bucket-a"))
        );
    }

    #[test]
    fn content_index_load_is_backward_compatible_with_the_old_space_separated_format() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(".content-hashes"),
            "hash-a pdf/report.pdf\n",
        )
        .unwrap();

        let index = ContentIndex::load(dir.path(), ".content-hashes").unwrap();

        assert_eq!(
            index.check_with_original_key_and_bucket("hash-a"),
            Some(("pdf/report.pdf", "", ""))
        );
    }

    #[test]
    fn content_index_load_is_backward_compatible_with_the_old_3_field_format() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(".content-hashes"),
            "hash-a\tpdf/report.pdf\tdocs/report.pdf\n",
        )
        .unwrap();

        let index = ContentIndex::load(dir.path(), ".content-hashes").unwrap();

        assert_eq!(
            index.check_with_original_key_and_bucket("hash-a"),
            Some(("pdf/report.pdf", "docs/report.pdf", ""))
        );
    }

    const MESSAGE_HASHES: &str = ".message-hashes";

    #[test]
    fn load_missing_file_is_empty() {
        let dir = tempfile::tempdir().unwrap();
        let index = ContentIndex::load(dir.path(), MESSAGE_HASHES).unwrap();
        assert!(index.check_with_original_key_and_bucket("abc").is_none());
    }

    #[test]
    fn load_skips_malformed_lines() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join(MESSAGE_HASHES), "no-space-here\n").unwrap();
        let index = ContentIndex::load(dir.path(), MESSAGE_HASHES).unwrap();
        assert!(
            index
                .check_with_original_key_and_bucket("no-space-here")
                .is_none()
        );
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
