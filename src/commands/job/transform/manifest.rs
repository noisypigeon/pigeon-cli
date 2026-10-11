//! Finds files already landed under `<local_output>/source/` (ADR-0112),
//! used as one of the pipeline's two dispatch sources alongside the pull
//! subprocess's live tail (`transform::worker`'s module doc comment).
//! "Already done" is no longer decided here -- `transform` runs on a
//! freshly-provisioned, ephemeral VM per invocation, so a local checkpoint
//! never survives a VM replacement and was proven dead weight in production
//! (ADR-0120). `worker::enqueue` now answers "already done" by checking
//! `destination::list_existing_filenames` instead, keyed off each file's
//! deterministic `placement::compute_destination_name`.

use std::path::{Path, PathBuf};

use super::format::InputFileType;
use crate::core::data::collect_files;

/// One pending file discovered under `<local_output>/source/`. `relative_path`
/// is relative to `source_dir`, using `/`-separated components regardless of
/// host OS -- this is also half of `placement`'s destination-name hash input
/// (ADR-0112 Decision §5).
pub(crate) struct PendingFile {
    pub relative_path: String,
    pub absolute_path: PathBuf,
}

/// `path`'s location relative to `base`, rendered with `/`-separated
/// components regardless of host OS -- used as half of
/// `placement::compute_destination_name`'s hash input, so it must be stable
/// across platforms for the same logical file.
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

/// Walks `source_dir` (everything the bulk pull has landed so far), keeping
/// only files whose extension matches `input_file_type` (defensive -- the
/// pull's own `--include` filter should already guarantee this; anything
/// else found here is unexpected, not fatal, and is skipped with a
/// `tracing::warn!`). Used as the pipeline's initial dispatch source
/// (`transform::worker`), run once before the pull subprocess spawns.
/// Whether a given file is actually already done is decided downstream by
/// `worker::enqueue`, not here.
pub(crate) fn gather_pending(
    source_dir: &Path,
    input_file_type: InputFileType,
) -> Result<Vec<PendingFile>, String> {
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
        pending.push(PendingFile {
            relative_path,
            absolute_path,
        });
    }
    Ok(pending)
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    #[test]
    fn gather_pending_filters_by_extension() {
        let dir = tempfile::tempdir().unwrap();
        let source_dir = dir.path().join("source");
        fs::create_dir_all(&source_dir).unwrap();
        fs::write(source_dir.join("a.png"), b"a").unwrap();
        fs::write(source_dir.join("stray.txt"), b"not a png").unwrap();

        let pending = gather_pending(&source_dir, InputFileType::Png).unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].relative_path, "a.png");
    }

    #[test]
    fn gather_pending_includes_a_nested_relative_path() {
        let dir = tempfile::tempdir().unwrap();
        let source_dir = dir.path().join("source");
        fs::create_dir_all(source_dir.join("screenshots")).unwrap();
        fs::write(source_dir.join("screenshots/img.heic"), b"x").unwrap();

        let pending = gather_pending(&source_dir, InputFileType::Heic).unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].relative_path, "screenshots/img.heic");
    }

    #[test]
    fn gather_pending_includes_every_matching_file_regardless_of_prior_state() {
        let dir = tempfile::tempdir().unwrap();
        let source_dir = dir.path().join("source");
        fs::create_dir_all(&source_dir).unwrap();
        fs::write(source_dir.join("a.png"), b"a").unwrap();
        fs::write(source_dir.join("b.png"), b"b").unwrap();

        // No checkpoint concept exists anymore -- gather_pending reports
        // every matching file every time; "already done" is decided by
        // `worker::enqueue` against the destination listing instead.
        let pending = gather_pending(&source_dir, InputFileType::Png).unwrap();
        assert_eq!(pending.len(), 2);
    }
}
