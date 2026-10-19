use std::collections::HashMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
/// One hash's committed entry: the final relative path, plus (CLI-0099) the
/// original source key of whichever file was kept under that path --
/// empty when the entry predates CLI-0099 or was committed by a caller that
/// only ever uses the plain `Dedup::commit` (original key is never
/// retroactively backfilled, since a hash is only ever committed once).
/// `source_bucket_alias` (CLI-0109) is likewise empty unless committed via
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
/// entries (optionally a 4th `\t<source-bucket-alias>` field, CLI-0109) --
/// a durable, append-only content-hash index backing byte-identical-content
/// deduplication (originally CLI-0012, for `pigeon email`'s message/
/// attachment dedup; genericized by CLI-0020 for reuse by other
/// transforms; relocated by CLI-0023; gained the tab-separated
/// `original-key` field in CLI-0099). The relative path stored is
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
    /// lines); a line with no tab falls back to the pre-CLI-0099
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
    /// file (CLI-0099) and the source bucket alias it was downloaded from
    /// (CLI-0109) -- both `""` when the entry predates the respective ADR
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
    /// bucket-config `original_key` was downloaded from (CLI-0109), as a
    /// 4th tab-separated field -- so a later duplicate's report row can
    /// name not just which key was kept, but which of this run's (possibly
    /// several) source buckets it came from. Only `deduplicate::dedup`
    /// calls this, and since CLI-0147 removed the two email jobs it is the
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
/// CLI-0013), caps its length (an externally-sourced name can be
/// arbitrarily long), and falls back to `"file"` if nothing usable remains.
/// Extension is preserved where reasonable, unlike
/// `email::identity::sanitize_segment`, which would corrupt it. Originally
/// CLI-0013's MIME-attachment-name fix; genericized by CLI-0020.
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
/// (CLI-0107), or on a source-tree relative path (`transform`, CLI-0130 --
/// these 82-byte stubs match an `--include '*.<ext>'` filter but are not
/// decodable media, and feeding one to `ffmpeg` produces a non-retryable
/// error that can trip the circuit breaker). Hoisted here on its second
/// consumer (CLI-0130), alongside this module's other filename predicates;
/// `commands::job::archive` is the other caller.
pub(crate) fn is_apple_double_basename(key: &str) -> bool {
    key.rsplit('/').next().unwrap_or(key).starts_with("._")
}

/// Whether `key`'s final path component is an Excel lock file (`~$<name>`),
/// the stub Excel leaves beside an open workbook (CLI-0149).
///
/// Same shape and the same two choke points as
/// `is_apple_double_basename`, and for the same reason: these match an
/// `--include '*.xlsx'` filter but are not workbooks, so without this they
/// arrive as *failures* rather than skips -- and a deterministic,
/// non-retryable failure class is exactly what can trip CLI-0121's circuit
/// breaker into aborting a whole run.
pub(crate) fn is_excel_lock_basename(key: &str) -> bool {
    key.rsplit('/').next().unwrap_or(key).starts_with("~$")
}

/// Longest a sheet-name slug may be, in bytes, inside a destination
/// filename (CLI-0149).
///
/// The budget: a `MAX_FILENAME_LENGTH` stem (100) + `-` + 16 hex digest +
/// `-` + a `NNofTT` marker (8 at three digits each) + `-` + this (40) +
/// `.csv` = 171 bytes, comfortably inside the 255-byte per-component limit
/// `MAX_FILENAME_LENGTH`'s own doc comment reasons about.
const MAX_SHEET_SLUG_LENGTH: usize = 40;

/// Reduces a workbook sheet name to a filename-safe, report-safe slug
/// (CLI-0149): keeps `[A-Za-z0-9._-]`, maps every other character to `-`,
/// collapses runs, trims leading/trailing `-`, truncates to
/// `MAX_SHEET_SLUG_LENGTH`, and falls back to `"sheet"` when nothing
/// usable remains.
///
/// Deliberately **stricter than `sanitize_filename`**, which does no
/// character filtering at all -- it keeps the final path component and
/// truncates, nothing more. That is sufficient for a filename but not
/// here, because this slug is also written into `transform-report.txt`,
/// whose format is tab-separated and one-line-per-row, and into that
/// report's comma-joined `destination_filenames` column. A sheet name
/// containing a tab, a newline, or a comma would corrupt all three.
/// Sheet names are author-controlled text, so this is the same class of
/// problem CLI-0033 fixed for `yaml_quote`, solved the same way: restrict
/// the character set at the producer rather than escape at each consumer.
///
/// The report's comma-join depends on this excluding `,`. Do not widen the
/// kept set without revisiting `transform::worker::write_report`.
pub(crate) fn sheet_slug(name: &str) -> String {
    slugify(name, MAX_SHEET_SLUG_LENGTH, "sheet")
}

/// A filename stem reduced to the same safe character set as `sheet_slug`,
/// at `MAX_FILENAME_LENGTH`'s larger budget (CLI-0151).
///
/// Used by `transform`'s `placement::group_key` for **spreadsheet input
/// kinds only**. The reason is narrow and worth stating: those are the only
/// kinds that can produce more than one output per input, so they are the
/// only ones whose `transform-report.txt` filename column holds a
/// *multi-value* list -- and a separator can only mislead when there is
/// more than one value to separate. CLI-0149 secured the sheet-name half of
/// such a name and missed this half, so a real run produced
/// `Budget (Normal, Reduced, Crisis)-ee7a9c8a4c2e5342.csv`: one file whose
/// own name reads as three comma-separated ones.
///
/// Deliberately **not** applied to the image, video, audio, or document
/// kinds.
/// `group_key` feeds every destination name this job has ever written, and
/// `transform` never deletes at the destination (CLI-0112), so widening
/// this would not rename those objects -- it would *duplicate* them.
pub(crate) fn strict_stem(name: &str) -> String {
    let base = match Path::new(name).file_name().and_then(|f| f.to_str()) {
        Some(base) if !base.is_empty() => base,
        _ => return "file".to_string(),
    };
    slugify(base, MAX_FILENAME_LENGTH, "file")
}

/// Keeps `[A-Za-z0-9._-]`, maps every other character to `-`, collapses
/// runs, trims, truncates to `max_len` bytes, and falls back to `fallback`
/// when nothing usable remains.
///
/// Truncation is byte-counted but can only cut between kept characters,
/// every one of which is single-byte ASCII, so the result is always valid
/// UTF-8 without a char-boundary dance.
fn slugify(name: &str, max_len: usize, fallback: &str) -> String {
    let mut slug = String::with_capacity(name.len().min(max_len));
    let mut pending_separator = false;
    for ch in name.chars() {
        if ch.is_ascii_alphanumeric() || ch == '.' || ch == '_' || ch == '-' {
            if pending_separator && !slug.is_empty() {
                slug.push('-');
            }
            pending_separator = false;
            if slug.len() >= max_len {
                break;
            }
            slug.push(ch);
        } else {
            pending_separator = true;
        }
    }
    let trimmed = slug.trim_matches('-');
    if trimmed.is_empty() {
        return fallback.to_string();
    }
    trimmed.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_excel_lock_basename_matches_only_an_excel_lock_stub() {
        assert!(is_excel_lock_basename("~$Budget.xlsx"));
        assert!(is_excel_lock_basename("finance/2026/~$Budget.xlsx"));
        assert!(!is_excel_lock_basename("Budget.xlsx"));
        assert!(!is_excel_lock_basename("finance/~tilde/Budget.xlsx"));
        // A `~$` anywhere but the start of the basename is an ordinary name.
        assert!(!is_excel_lock_basename("Budget~$.xlsx"));
        assert!(!is_excel_lock_basename(""));
    }

    /// CLI-0151: the stem half of a destination name needs the same
    /// treatment the slug half got, at the larger `MAX_FILENAME_LENGTH`
    /// budget. The real run that prompted this produced
    /// `Budget (Normal, Reduced, Crisis)-ee7a9c8a4c2e5342.csv` -- one file
    /// whose own name reads as three comma-separated ones.
    #[test]
    fn strict_stem_removes_what_sanitize_filename_lets_through() {
        assert_eq!(
            strict_stem("Budget (Normal, Reduced, Crisis)"),
            "Budget-Normal-Reduced-Crisis"
        );
        // The exact failure: `sanitize_filename` keeps the commas.
        assert!(sanitize_filename("Budget (Normal, Reduced, Crisis)").contains(','));
        assert!(!strict_stem("Budget (Normal, Reduced, Crisis)").contains(','));

        for name in ["a,b", "a\tb", "a\nb", "a b", "a/b"] {
            let stem = strict_stem(name);
            assert!(!stem.contains(','), "{stem:?}");
            assert!(!stem.contains('\t'), "{stem:?}");
            assert!(!stem.contains('\n'), "{stem:?}");
            assert!(!stem.contains('/'), "{stem:?}");
        }
    }

    #[test]
    fn strict_stem_keeps_an_already_safe_name_unchanged() {
        for name in [
            "2021_quebec_inc_td_cc_7652",
            "money_schema_v3.2",
            "already-hyphenated",
        ] {
            assert_eq!(strict_stem(name), name);
        }
    }

    /// A stem gets `MAX_FILENAME_LENGTH`, not the slug's tighter budget --
    /// the two share a character set, not a length.
    #[test]
    fn strict_stem_uses_the_filename_budget_not_the_slug_budget() {
        let stem = strict_stem(&"x".repeat(300));
        assert_eq!(stem.len(), MAX_FILENAME_LENGTH);
        assert!(stem.len() > MAX_SHEET_SLUG_LENGTH);
        assert_eq!(strict_stem(""), "file");
        assert_eq!(strict_stem("///"), "file");
    }

    #[test]
    fn sheet_slug_keeps_safe_characters_and_collapses_the_rest() {
        assert_eq!(sheet_slug("Summary"), "Summary");
        assert_eq!(sheet_slug("Q1 Detail"), "Q1-Detail");
        assert_eq!(sheet_slug("2026_totals.v2"), "2026_totals.v2");
        assert_eq!(sheet_slug("already-hyphenated"), "already-hyphenated");
        // Runs of unsafe characters collapse to a single separator.
        assert_eq!(sheet_slug("A   ///   B"), "A-B");
        // Leading/trailing separators are trimmed, never left dangling.
        assert_eq!(sheet_slug("  Summary  "), "Summary");
        assert_eq!(sheet_slug("///"), "sheet");
        assert_eq!(sheet_slug(""), "sheet");
    }

    /// The reason this exists rather than reusing `sanitize_filename`
    /// (CLI-0149): the slug is written into `transform-report.txt`, which is
    /// tab-separated and one-line-per-row, and into that report's
    /// comma-joined filename column. A sheet name is author-controlled, so
    /// a tab, newline, or comma reaching the slug would corrupt the report.
    #[test]
    fn sheet_slug_strips_characters_that_would_corrupt_the_report() {
        for (name, expected) in [
            ("has\ttab", "has-tab"),
            ("has\nnewline", "has-newline"),
            ("has\r\ncrlf", "has-crlf"),
            ("Q1,Q2", "Q1-Q2"),
            ("a\u{1}soh", "a-soh"),
        ] {
            let slug = sheet_slug(name);
            assert_eq!(slug, expected, "slug for {name:?}");
            assert!(!slug.contains('\t'), "{slug:?} kept a tab");
            assert!(!slug.contains('\n'), "{slug:?} kept a newline");
            assert!(!slug.contains(','), "{slug:?} kept a comma");
        }
        // `sanitize_filename`, by contrast, passes all of them straight
        // through -- which is exactly why it is not used for this.
        assert!(sanitize_filename("Q1,Q2").contains(','));
    }

    /// A path separator must never survive, or `Path::join` would create an
    /// implicit subdirectory -- CLI-0013's original concern, which this
    /// slug inherits by restricting the character set rather than by
    /// taking the final component.
    #[test]
    fn sheet_slug_cannot_emit_a_path_separator() {
        assert_eq!(sheet_slug("a/b"), "a-b");
        assert_eq!(sheet_slug("../escape"), "..-escape");
        assert!(!sheet_slug("a/../b").contains('/'));
    }

    #[test]
    fn sheet_slug_truncates_to_the_budgeted_length() {
        let slug = sheet_slug(&"x".repeat(200));
        assert_eq!(slug.len(), MAX_SHEET_SLUG_LENGTH);
        // Non-ASCII must not be cut mid-character; the filter drops it
        // entirely, so the result stays valid UTF-8 either way.
        let wide = sheet_slug(&"\u{e9}".repeat(200));
        assert_eq!(wide, "sheet");
    }

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
