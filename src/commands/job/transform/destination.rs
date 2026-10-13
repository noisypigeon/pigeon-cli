//! Lists what already exists at `--destination-bucket` via `rclone lsjson`
//! (ADR-0120) -- the authoritative, persistent source of "is this file
//! already done" for `transform`, replacing the old local `.staging/
//! .processed` checkpoint. `transform` runs on a freshly-provisioned,
//! ephemeral VM per invocation: local disk never survives a VM replacement,
//! but `--destination-bucket` does, and `placement::compute_destination_name`
//! already makes "already done" a deterministic, checkable question against
//! it -- the same source file always maps to the same destination filename
//! on every run.
//!
//! Also lists the *source* tree the same way (ADR-0124) -- a metadata-only
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
        job: "transform",
        ..Default::default()
    })
    .run()
    .await?;
    Ok(entries.into_iter().map(|entry| entry.name).collect())
}

/// Lists every relative path under `source` matching `extension` (the same
/// single-extension scope Phase A's own pull already applies), via a
/// recursive, metadata-only `rclone lsjson` -- no file content is read or
/// transferred. A source that doesn't exist (or exists but is empty)
/// resolves to an empty list, same handling as `list_existing_filenames`.
/// Relative paths use `/`-separated components, matching
/// `manifest::relative_path_string`'s convention -- both are half of
/// `placement::compute_destination_name`'s hash input, so they must agree
/// for the same logical file.
pub(crate) async fn list_source_relative_paths(
    source: &str,
    extension: &str,
) -> Result<Vec<String>, String> {
    let entries = (Lsjson {
        path: source.to_string(),
        recursive: true,
        include_extension: Some(extension.to_string()),
        job: "transform",
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

        let paths = list_source_relative_paths(dir.path().to_str().unwrap(), "png")
            .await
            .unwrap();

        assert_eq!(paths, vec!["screenshots/img.png".to_string()]);
    }

    #[tokio::test]
    async fn list_source_relative_paths_returns_empty_for_a_source_that_does_not_exist_yet() {
        if check_rclone_available().await.is_err() {
            eprintln!("skipping: rclone not found on PATH");
            return;
        }

        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("does-not-exist-yet");

        let paths = list_source_relative_paths(missing.to_str().unwrap(), "png")
            .await
            .unwrap();

        assert!(paths.is_empty());
    }
}
