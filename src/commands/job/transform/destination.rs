//! Lists what already exists at `--destination-path` via `rclone lsjson`
//! (ADR-0120) -- the authoritative, persistent source of "is this file
//! already done" for `transform`, replacing the old local `.staging/
//! .processed` checkpoint. `transform` runs on a freshly-provisioned,
//! ephemeral VM per invocation: local disk never survives a VM replacement,
//! but `--destination-path` does, and `placement::compute_destination_name`
//! already makes "already done" a deterministic, checkable question against
//! it -- the same source file always maps to the same destination filename
//! on every run.

use std::collections::HashSet;
use std::time::Duration;

use serde::Deserialize;

use crate::core::retry::retry_with_backoff;

/// Same shape as `push.rs`'s `PUSH_RETRIES`/`PUSH_RETRY_BACKOFF` -- a
/// connection-level failure before rclone's own internal retry logic can
/// engage is the thing this outer layer catches, not a correctness
/// mechanism in its own right.
const DESTINATION_LISTING_RETRIES: usize = 3;
const DESTINATION_LISTING_RETRY_BACKOFF: Duration = Duration::from_secs(2);

#[derive(Debug, Deserialize)]
struct LsjsonEntry {
    #[serde(rename = "Name")]
    name: String,
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
    retry_with_backoff(
        DESTINATION_LISTING_RETRIES,
        DESTINATION_LISTING_RETRY_BACKOFF,
        || run_lsjson(destination_path),
    )
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
}
