//! Destination filenames unique by construction, not collision-detected-
//! and-fixed (ADR-0112 Decision §5). `core::data::unique_path` (the
//! `-2`/`-3` suffix-on-existing-name scheme `deduplicate`/`pull_transform`
//! use) only checks what's already present in the *local* `result/`
//! directory -- it has no visibility into whatever the remote
//! `--destination-path` already contains from an earlier run, so it can
//! only guarantee run-local uniqueness, never global uniqueness. This
//! module never calls it.

use std::path::{Path, PathBuf};

use crate::commands::job::download;
use crate::core::data::sanitize_filename;

/// How many hex characters (of the full SHA-256 digest) to keep in a
/// destination name -- 64 bits, a collision probability negligible at this
/// job's realistic scale (a run processing low millions of files would
/// still need to be extraordinarily unlucky to collide).
const NAME_HASH_HEX_LEN: usize = 16;

/// Derives a destination filename deterministically from
/// `(source_path, original_relative_path)`: unique by construction, with no
/// filesystem probing, no ordering-dependence, and no blind spot against
/// pre-existing destination content. Hashing the *full* relative path (not
/// just the basename) means two files that only differ by directory (e.g.
/// `screenshots/IMG_0001.png` vs. `photos/IMG_0001.png`) never collide;
/// including `source_path` in the hash input means two different source
/// trees with coincidentally identical relative paths never collide
/// either. The same source file always maps to the exact same destination
/// name on every run, which is also what makes a `transform` rerun line up
/// cleanly with Phase C's `rclone copy` incremental push-skip.
pub(crate) fn compute_destination_name(source_path: &str, original_relative_path: &str) -> String {
    let stem = sanitize_filename(
        Path::new(original_relative_path)
            .file_stem()
            .and_then(|stem| stem.to_str())
            .unwrap_or("file"),
    );
    let digest =
        download::sha256_hex(format!("{source_path}\u{1}{original_relative_path}").as_bytes());
    format!("{stem}-{}.jpg", &digest[..NAME_HASH_HEX_LEN])
}

/// Moves `scratch_path` (the already-transcoded or copied-through file)
/// into `result_dir` under its computed destination name. If that path
/// already exists, this is a **fatal error**, not something to silently
/// resolve with a suffix -- at this job's scale a collision indicates
/// either a genuine bug or an astronomically unlikely hash collision, and a
/// verifiably-unique naming scheme should fail loudly rather than quietly
/// falling back to probabilistic disambiguation (which would defeat the
/// point of having one).
pub(crate) fn place_one(
    result_dir: &Path,
    scratch_path: &Path,
    source_path: &str,
    original_relative_path: &str,
) -> Result<PathBuf, String> {
    std::fs::create_dir_all(result_dir)
        .map_err(|err| format!("failed to create {}: {err}", result_dir.display()))?;
    let final_path = result_dir.join(compute_destination_name(
        source_path,
        original_relative_path,
    ));
    if final_path.exists() {
        return Err(format!(
            "destination name collision placing '{original_relative_path}' at {} -- \
             this should be statistically impossible with the current naming scheme \
             and indicates a bug",
            final_path.display()
        ));
    }
    std::fs::rename(scratch_path, &final_path).map_err(|err| {
        format!(
            "failed to move {} to {}: {err}",
            scratch_path.display(),
            final_path.display()
        )
    })?;
    Ok(final_path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compute_destination_name_is_deterministic() {
        let first = compute_destination_name("source:png/", "screenshots/IMG_0001.png");
        let second = compute_destination_name("source:png/", "screenshots/IMG_0001.png");
        assert_eq!(first, second);
    }

    #[test]
    fn compute_destination_name_differs_for_relative_paths_sharing_a_basename() {
        let a = compute_destination_name("source:png/", "screenshots/IMG_0001.png");
        let b = compute_destination_name("source:png/", "photos/IMG_0001.png");
        assert_ne!(a, b);
        // Both still carry the shared basename as a readable prefix.
        assert!(a.starts_with("IMG_0001-"));
        assert!(b.starts_with("IMG_0001-"));
    }

    #[test]
    fn compute_destination_name_differs_across_source_paths_with_an_identical_relative_path() {
        let a = compute_destination_name("source-a:png/", "IMG_0001.png");
        let b = compute_destination_name("source-b:png/", "IMG_0001.png");
        assert_ne!(a, b);
    }

    #[test]
    fn compute_destination_name_always_ends_in_jpg() {
        let name = compute_destination_name("source:heic/", "a/b/c.heic");
        assert!(name.ends_with(".jpg"));
    }

    #[test]
    fn place_one_moves_the_scratch_file_into_result_dir() {
        let dir = tempfile::tempdir().unwrap();
        let result_dir = dir.path().join("result");
        let scratch_path = dir.path().join("scratch.jpg");
        std::fs::write(&scratch_path, b"jpeg bytes").unwrap();

        let final_path = place_one(&result_dir, &scratch_path, "source:png/", "a.png").unwrap();

        assert!(final_path.exists());
        assert!(!scratch_path.exists());
        assert_eq!(std::fs::read(&final_path).unwrap(), b"jpeg bytes");
    }

    #[test]
    fn place_one_hard_errors_on_a_forced_collision_instead_of_silently_renaming() {
        let dir = tempfile::tempdir().unwrap();
        let result_dir = dir.path().join("result");
        std::fs::create_dir_all(&result_dir).unwrap();
        let colliding_name = compute_destination_name("source:png/", "a.png");
        std::fs::write(result_dir.join(&colliding_name), b"already here").unwrap();

        let scratch_path = dir.path().join("scratch.jpg");
        std::fs::write(&scratch_path, b"new content").unwrap();

        let result = place_one(&result_dir, &scratch_path, "source:png/", "a.png");
        assert!(result.is_err());
        // The pre-existing file must be untouched -- this is a hard error,
        // not a silent overwrite or a `-2` suffix fallback.
        assert_eq!(
            std::fs::read(result_dir.join(&colliding_name)).unwrap(),
            b"already here"
        );
    }
}
