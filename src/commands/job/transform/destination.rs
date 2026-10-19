//! Lists what already exists at `--destination-bucket` via `rclone lsjson`
//! (CLI-0120) -- the authoritative, persistent source of "is this file
//! already done" for `transform`, replacing the old local `.staging/
//! .processed` checkpoint. `transform` runs on a freshly-provisioned,
//! ephemeral VM per invocation: local disk never survives a VM replacement,
//! but `--destination-bucket` does, and `placement::compute_destination_name`
//! already makes "already done" a deterministic, checkable question against
//! it -- the same source file always maps to the same destination filename
//! on every run.
//!
//! Also lists the *source* tree the same way (CLI-0124) -- a metadata-only
//! `rclone lsjson`, no bytes transferred -- so `worker::run_transform_job`
//! can compute which source files are already done (via
//! `placement::compute_destination_name`, a pure function of a relative
//! path alone) *before* Phase A's pull ever starts, rather than discovering
//! it only after redundantly downloading them.

use std::collections::HashSet;

use crate::core::rclone::RcloneCommand as _;
use crate::core::rclone::command::Lsjson;

/// Lists every filename already present at `destination` (flat, not
/// recursive -- `transform`'s `result/` tree and therefore its destination
/// are always flat, per `placement.rs`). A destination that doesn't exist
/// yet (the very first run against a brand-new bucket) is treated as
/// empty, not an error (`Lsjson::finish`).
pub(crate) async fn list_existing_filenames(destination: &str) -> Result<HashSet<String>, String> {
    let entries = (Lsjson {
        path: destination.to_string(),
        // The one listing in this crate that may legitimately find nothing
        // at a path rclone cannot resolve (CLI-0140).
        missing_ok: true,
        job: "transform",
        ..Default::default()
    })
    .run()
    .await?;
    Ok(entries.into_iter().map(|entry| entry.name).collect())
}

/// Lists every relative path under `source` matching any of `extensions`
/// (one input kind's full set of on-disk spellings -- `.jpg` *and* `.jpeg`
/// for `Jpeg`, per CLI-0139), via a recursive, metadata-only `rclone
/// lsjson` -- no file content is read or transferred. A source rclone
/// cannot resolve at all is an `Err` here (CLI-0140), unlike
/// `list_existing_filenames`; a source that resolves but holds nothing
/// matching is an empty list.
///
/// This listing is the **sole** input to the pull set: an empty result
/// means `worker::run_transform_job` skips spawning `rclone` entirely, so a
/// filter that matches nothing is indistinguishable from an empty source.
/// That is precisely how CLI-0139's bug stayed invisible, and why the
/// caller warns on an empty result.
///
/// Relative paths use `/`-separated components, matching
/// `manifest::relative_path_string`'s convention -- both are half of
/// `placement::compute_destination_name`'s hash input, so they must agree
/// for the same logical file.
pub(crate) async fn list_source_relative_paths(
    source: &str,
    extensions: &[&str],
) -> Result<Vec<String>, String> {
    let entries = (Lsjson {
        path: source.to_string(),
        recursive: true,
        include_extensions: extensions.iter().map(|ext| ext.to_string()).collect(),
        // Deliberately *not* `missing_ok` (CLI-0140): a source rclone cannot
        // resolve is a misconfigured address, not an empty source.
        missing_ok: false,
        job: "transform",
        ..Default::default()
    })
    .run()
    .await?;
    Ok(entries.into_iter().map(|entry| entry.path).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn check_rclone_available() -> Result<(), String> {
        crate::core::rclone::check_available().await
    }

    #[tokio::test]
    async fn list_existing_filenames_lists_files_present_in_a_local_directory() {
        if check_rclone_available().await.is_err() {
            eprintln!("skipping: rclone not found on PATH");
            return;
        }

        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path()
                .join("2021-08-20_Delivery_Status_Notification_(Failure)_1.png"),
            b"png bytes",
        )
        .unwrap();
        std::fs::write(dir.path().join("plain.jpg"), b"jpeg bytes").unwrap();

        let names = list_existing_filenames(dir.path().to_str().unwrap())
            .await
            .unwrap();

        assert!(names.contains("2021-08-20_Delivery_Status_Notification_(Failure)_1.png"));
        assert!(names.contains("plain.jpg"));
        assert_eq!(names.len(), 2);
    }

    #[tokio::test]
    async fn list_existing_filenames_returns_empty_for_a_destination_that_does_not_exist_yet() {
        if check_rclone_available().await.is_err() {
            eprintln!("skipping: rclone not found on PATH");
            return;
        }

        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("does-not-exist-yet");

        let names = list_existing_filenames(missing.to_str().unwrap())
            .await
            .unwrap();

        assert!(names.is_empty());
    }

    #[tokio::test]
    async fn list_source_relative_paths_lists_nested_files_matching_the_extension() {
        if check_rclone_available().await.is_err() {
            eprintln!("skipping: rclone not found on PATH");
            return;
        }

        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("screenshots")).unwrap();
        std::fs::write(dir.path().join("screenshots/img.png"), b"png bytes").unwrap();
        std::fs::write(dir.path().join("notes.txt"), b"not a png").unwrap();

        let paths = list_source_relative_paths(dir.path().to_str().unwrap(), &["png"])
            .await
            .unwrap();

        assert_eq!(paths, vec!["screenshots/img.png".to_string()]);
    }

    /// The CLI-0139 regression, against a real `rclone`: one input kind
    /// with two spellings must match both. `--ignore-case` alone never
    /// could -- it folds `.JPG` to `.jpg`, but cannot fold `jpg` to `jpeg`.
    #[tokio::test]
    async fn list_source_relative_paths_matches_every_spelling_of_one_input_kind() {
        if check_rclone_available().await.is_err() {
            eprintln!("skipping: rclone not found on PATH");
            return;
        }

        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.jpg"), b"jpg bytes").unwrap();
        std::fs::write(dir.path().join("b.JPG"), b"uppercase jpg bytes").unwrap();
        std::fs::write(dir.path().join("c.jpeg"), b"jpeg bytes").unwrap();
        std::fs::write(dir.path().join("d.png"), b"a different format").unwrap();

        let mut paths = list_source_relative_paths(dir.path().to_str().unwrap(), &["jpg", "jpeg"])
            .await
            .unwrap();
        paths.sort();

        assert_eq!(
            paths,
            vec![
                "a.jpg".to_string(),
                "b.JPG".to_string(),
                "c.jpeg".to_string()
            ],
            "every jpg/jpeg spelling must match, and only those"
        );
    }

    /// CLI-0140: the inverse of `list_existing_filenames`' tolerance. A
    /// source rclone cannot resolve used to come back as an empty listing,
    /// which `run_transform_job` turns into a run that pulls nothing and
    /// exits 0 -- indistinguishable from success.
    #[tokio::test]
    async fn list_source_relative_paths_errors_for_a_source_that_does_not_exist() {
        if check_rclone_available().await.is_err() {
            eprintln!("skipping: rclone not found on PATH");
            return;
        }

        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("does-not-exist-yet");

        let err = list_source_relative_paths(missing.to_str().unwrap(), &["png"])
            .await
            .unwrap_err();

        assert!(
            err.contains("does-not-exist-yet"),
            "the unresolvable source must be named in the error: {err}"
        );
    }
}
