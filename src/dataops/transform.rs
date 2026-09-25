use std::fs;
use std::path::{Path, PathBuf};

/// Escapes `s` as a double-quoted YAML scalar.
pub(crate) fn yaml_quote(s: &str) -> String {
    let escaped = s.replace('\\', "\\\\").replace('"', "\\\"");
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
