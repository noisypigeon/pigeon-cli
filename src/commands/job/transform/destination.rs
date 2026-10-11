//! Lists what already exists at `--destination-path` via `rclone lsjson`
//! (ADR-0120) -- the authoritative, persistent source of "is this file
//! already done" for `transform`, replacing the old local `.staging/
//! .processed` checkpoint. `transform` runs on a freshly-provisioned,
//! ephemeral VM per invocation: local disk never survives a VM replacement,
//! but `--destination-path` does, and `placement::compute_destination_name`
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
use std::time::Duration;

use serde::Deserialize;

use crate::core::retry::retry_with_backoff;

/// Shared by both the destination and source listings below -- a
/// connection-level failure before rclone's own internal retry logic can
/// engage is the thing this outer layer catches, not a correctness
/// mechanism in its own right. Same shape as `push.rs`'s
/// `PUSH_RETRIES`/`PUSH_RETRY_BACKOFF`.
const LSJSON_RETRIES: usize = 3;
const LSJSON_RETRY_BACKOFF: Duration = Duration::from_secs(2);

#[derive(Debug, Deserialize)]
struct LsjsonEntry {
    #[serde(rename = "Name")]
    name: String,
}

#[derive(Debug, Deserialize)]
struct LsjsonPathEntry {
    #[serde(rename = "Path")]
    path: String,
}

/// Lists every filename already present at `destination_path` (flat, not
/// recursive -- `transform`'s `result/` tree and therefore its destination
/// are always flat, per `placement.rs`). A destination that doesn't exist
/// yet (the very first run against a brand-new path) is treated as empty,
/// not an error -- confirmed against a real `rclone v1.72.1` local-backend
/// run: a missing directory exits non-zero with `"directory not found"` in
/// stderr. Any other non-zero exit is a genuine error. Pattern-matching
/// subprocess stderr is inherently version-fragile -- reconfirm this text
/// against whatever `rclone` version is actually deployed before relying on
/// it in production.
pub(crate) async fn list_existing_filenames(
    destination_path: &str,
) -> Result<HashSet<String>, String> {
    retry_with_backoff(LSJSON_RETRIES, LSJSON_RETRY_BACKOFF, || {
        run_lsjson(destination_path)
    })
    .await
}

/// Lists every relative path under `source_path` matching `extension`
/// (the same single-extension scope Phase A's own pull already applies),
/// via a recursive, metadata-only `rclone lsjson` -- no file content is
/// read or transferred. A source path that doesn't exist (or exists but is
/// empty) resolves to an empty list, same "directory not found" handling as
/// `run_lsjson` below. Relative paths use `/`-separated components,
/// matching `manifest::relative_path_string`'s convention -- both are half
/// of `placement::compute_destination_name`'s hash input, so they must
/// agree for the same logical file.
pub(crate) async fn list_source_relative_paths(
    source_path: &str,
    extension: &str,
) -> Result<Vec<String>, String> {
    retry_with_backoff(LSJSON_RETRIES, LSJSON_RETRY_BACKOFF, || {
        run_source_lsjson(source_path, extension)
    })
    .await
}

async fn run_lsjson(destination_path: &str) -> Result<HashSet<String>, String> {
    let output = tokio::process::Command::new("rclone")
        .arg("lsjson")
        .args(["--files-only", "--no-modtime", "--no-mimetype"])
        .arg(destination_path)
        .output()
        .await
        .map_err(|err| format!("failed to run rclone lsjson: {err}"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        if stderr.to_ascii_lowercase().contains("directory not found") {
            return Ok(HashSet::new());
        }
        return Err(format!(
            "rclone lsjson {destination_path} exited with status {}: {}",
            output
                .status
                .code()
                .map(|code| code.to_string())
                .unwrap_or_else(|| "unknown (terminated by signal)".to_string()),
            stderr.trim(),
        ));
    }

    let entries: Vec<LsjsonEntry> = serde_json::from_slice(&output.stdout).map_err(|err| {
        format!("failed to parse rclone lsjson output for {destination_path}: {err}")
    })?;
    Ok(entries.into_iter().map(|entry| entry.name).collect())
}

async fn run_source_lsjson(source_path: &str, extension: &str) -> Result<Vec<String>, String> {
    let output = tokio::process::Command::new("rclone")
        .arg("lsjson")
        .args([
            "--recursive",
            "--files-only",
            "--no-modtime",
            "--no-mimetype",
        ])
        .arg("--include")
        .arg(format!("*.{extension}"))
        .arg("--ignore-case")
        .arg(source_path)
        .output()
        .await
        .map_err(|err| format!("failed to run rclone lsjson: {err}"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        if stderr.to_ascii_lowercase().contains("directory not found") {
            return Ok(Vec::new());
        }
        return Err(format!(
            "rclone lsjson {source_path} exited with status {}: {}",
            output
                .status
                .code()
                .map(|code| code.to_string())
                .unwrap_or_else(|| "unknown (terminated by signal)".to_string()),
            stderr.trim(),
        ));
    }

    let entries: Vec<LsjsonPathEntry> = serde_json::from_slice(&output.stdout)
        .map_err(|err| format!("failed to parse rclone lsjson output for {source_path}: {err}"))?;
    Ok(entries.into_iter().map(|entry| entry.path).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn check_rclone_available() -> Result<(), String> {
        tokio::process::Command::new("rclone")
            .arg("version")
            .output()
            .await
            .map_err(|_| "rclone not found on PATH".to_string())?;
        Ok(())
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
