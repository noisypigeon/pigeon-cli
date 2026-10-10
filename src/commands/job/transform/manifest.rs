//! Phase B's (and only Phase B's) checkpoint (ADR-0112 Decision §6): Phase A
//! (`rclone copy` pull) and Phase C (`rclone copy` push) are already
//! incrementally idempotent on their own via rclone's own size/modtime-based
//! skip, so neither needs a pigeon-level checkpoint. Placement is the one
//! side effect that is not naturally rerun-safe on its own (it mints a new
//! file under `result/` every time it runs), so this is the only checkpoint
//! `transform` needs.

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

use super::format::InputFileType;
use crate::core::data::collect_files;

pub(crate) const PROCESSED_FILE_NAME: &str = ".processed";

/// One pending file discovered under `<local_output>/source/`, not yet
/// marked done in the Phase B checkpoint. `relative_path` is relative to
/// `source_dir`, using `/`-separated components regardless of host OS --
/// this is also the checkpoint key and half of `placement`'s destination-
/// name hash input (ADR-0112 Decision §5).
pub(crate) struct PendingFile {
    pub relative_path: String,
    pub absolute_path: PathBuf,
}

/// One relative-path-per-line checkpoint under `staging_dir` (always
/// `<local_output>/.staging/`, never `<local_output>/result/`, so it's
/// never swept into Phase C's push by `collect_files`). A missing file is
/// an empty, not-yet-started checkpoint, same convention as every other
/// job's `.processed`.
pub(crate) fn load_checkpoint(staging_dir: &Path) -> Result<HashSet<String>, String> {
    let path = staging_dir.join(PROCESSED_FILE_NAME);
    let contents = match fs::read_to_string(&path) {
        Ok(contents) => contents,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(HashSet::new()),
        Err(err) => return Err(format!("failed to read {}: {err}", path.display())),
    };
    Ok(contents.lines().map(|line| line.to_string()).collect())
}

/// Appended only once `relative_path`'s placement under `result/` has fully
/// succeeded -- never before, so a crash mid-transcode never falsely
/// checkpoints a file that was never actually placed.
pub(crate) fn append_checkpoint(staging_dir: &Path, relative_path: &str) -> Result<(), String> {
    use std::io::Write;
    fs::create_dir_all(staging_dir)
        .map_err(|err| format!("failed to create {}: {err}", staging_dir.display()))?;
    let path = staging_dir.join(PROCESSED_FILE_NAME);
    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .map_err(|err| format!("failed to open {}: {err}", path.display()))?;
    writeln!(file, "{relative_path}")
        .map_err(|err| format!("failed to write {}: {err}", path.display()))
}

/// `path`'s location relative to `base`, rendered with `/`-separated
/// components regardless of host OS -- used both as the checkpoint key and
/// as half of `placement::compute_destination_name`'s hash input, so it
/// must be stable across platforms for the same logical file.
fn relative_path_string(base: &Path, path: &Path) -> Option<String> {
    let relative = path.strip_prefix(base).ok()?;
    let components: Vec<&str> = relative
        .components()
        .filter_map(|c| c.as_os_str().to_str())
        .collect();
    if components.is_empty() {
        return None;
    }
    Some(components.join("/"))
}

/// Walks `source_dir` (everything Phase A pulled), keeps only files whose
/// extension matches `input_file_type` (defensive -- Phase A's own
/// `--include` filter should already guarantee this; anything else found
/// here is unexpected, not fatal, and is skipped with a `tracing::warn!`),
/// and excludes anything the Phase B checkpoint already marks done.
pub(crate) fn gather_pending(
    source_dir: &Path,
    staging_dir: &Path,
    input_file_type: InputFileType,
) -> Result<Vec<PendingFile>, String> {
    let done = load_checkpoint(staging_dir)?;
    let expected_extension = input_file_type.extension();

    let mut pending = Vec::new();
    for absolute_path in collect_files(source_dir)? {
        let Some(relative_path) = relative_path_string(source_dir, &absolute_path) else {
            continue;
        };
        let actual_extension = absolute_path
            .extension()
            .and_then(|ext| ext.to_str())
            .map(|ext| ext.to_ascii_lowercase());
        if actual_extension.as_deref() != Some(expected_extension) {
            tracing::warn!(
                path = %absolute_path.display(),
                expected = expected_extension,
                "transform: found an unexpected extension under source/, skipping"
            );
            continue;
        }
        if done.contains(&relative_path) {
            continue;
        }
        pending.push(PendingFile {
            relative_path,
            absolute_path,
        });
    }
    Ok(pending)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn load_checkpoint_is_empty_for_a_missing_staging_dir() {
        let dir = tempfile::tempdir().unwrap();
        let staging_dir = dir.path().join("does-not-exist-yet");
        assert!(load_checkpoint(&staging_dir).unwrap().is_empty());
    }

    #[test]
    fn append_checkpoint_then_load_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let staging_dir = dir.path().join(".staging");
        append_checkpoint(&staging_dir, "a.png").unwrap();
        append_checkpoint(&staging_dir, "sub/b.png").unwrap();

        let loaded = load_checkpoint(&staging_dir).unwrap();
        assert!(loaded.contains("a.png"));
        assert!(loaded.contains("sub/b.png"));
        assert_eq!(loaded.len(), 2);
    }

    #[test]
    fn gather_pending_excludes_checkpointed_files() {
        let dir = tempfile::tempdir().unwrap();
        let source_dir = dir.path().join("source");
        let staging_dir = dir.path().join(".staging");
        fs::create_dir_all(&source_dir).unwrap();
        fs::write(source_dir.join("a.png"), b"a").unwrap();
        fs::write(source_dir.join("b.png"), b"b").unwrap();
        append_checkpoint(&staging_dir, "a.png").unwrap();

        let pending = gather_pending(&source_dir, &staging_dir, InputFileType::Png).unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].relative_path, "b.png");
    }

    #[test]
    fn gather_pending_filters_by_extension() {
        let dir = tempfile::tempdir().unwrap();
        let source_dir = dir.path().join("source");
        let staging_dir = dir.path().join(".staging");
        fs::create_dir_all(&source_dir).unwrap();
        fs::write(source_dir.join("a.png"), b"a").unwrap();
        fs::write(source_dir.join("stray.txt"), b"not a png").unwrap();

        let pending = gather_pending(&source_dir, &staging_dir, InputFileType::Png).unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].relative_path, "a.png");
    }

    #[test]
    fn gather_pending_includes_a_nested_relative_path() {
        let dir = tempfile::tempdir().unwrap();
        let source_dir = dir.path().join("source");
        let staging_dir = dir.path().join(".staging");
        fs::create_dir_all(source_dir.join("screenshots")).unwrap();
        fs::write(source_dir.join("screenshots/img.heic"), b"x").unwrap();

        let pending = gather_pending(&source_dir, &staging_dir, InputFileType::Heic).unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].relative_path, "screenshots/img.heic");
    }
}
