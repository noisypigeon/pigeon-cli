//! Per-file push via `rclone copyto` (ADR-0116) -- replaces the old bulk
//! Phase C `rclone copy` of the whole `result/` tree. `copyto <src-file>
//! <dest-file>` is rclone's documented single-file-to-single-file-path
//! primitive: no directory listing round trip for one named object, unlike
//! the bulk-tree `copy` the pull phase still uses. Pushing immediately after
//! each file transcodes (rather than waiting for the whole batch) is the
//! other half of the fix for a single bad file silently discarding every
//! already-succeeded file's push (ADR-0116's Context).

use std::path::{Path, PathBuf};

use crate::core::rclone::RcloneCommand as _;
use crate::core::rclone::command::CopyTo;

/// Pushes one already-placed file to `<destination>/<destination_filename>`.
/// `push_log_dir` holds one small JSON log per file (never one shared log
/// path -- many concurrent single-file `rclone` subprocesses cannot safely
/// share one `--log-file`), purely for diagnostics on failure.
/// Where `push_one` writes this output's own rclone log.
///
/// Exposed so `worker` can delete it alongside the output once the push
/// succeeds (ADR-0151) -- there is one of these per output object and
/// nothing used to remove them.
pub(crate) fn log_path(push_log_dir: &Path, destination_filename: &str) -> PathBuf {
    push_log_dir.join(format!("{destination_filename}.jsonl"))
}

pub(crate) async fn push_one(
    local_path: &Path,
    destination: &str,
    destination_filename: &str,
    push_log_dir: &Path,
) -> Result<(), String> {
    let destination = format!(
        "{}/{destination_filename}",
        destination.trim_end_matches('/')
    );
    let log_path = log_path(push_log_dir, destination_filename);

    // Read before the push: ADR-0124's GC deletes this file immediately
    // after a successful push, so its size is unavailable afterwards.
    let bytes = std::fs::metadata(local_path)
        .map(|meta| meta.len())
        .unwrap_or(0);

    (CopyTo {
        source: local_path.display().to_string(),
        destination,
        log_path,
        job: "transform",
        step: "push",
        emit_upload_metrics: true,
        bytes,
    })
    .run()
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn check_rclone_available() -> Result<(), String> {
        crate::core::rclone::check_available().await
    }

    #[tokio::test]
    async fn push_one_copies_a_file_to_a_local_destination_path() {
        if check_rclone_available().await.is_err() {
            eprintln!("skipping: rclone not found on PATH");
            return;
        }

        let source_dir = tempfile::tempdir().unwrap();
        let dest_dir = tempfile::tempdir().unwrap();
        let local_path = source_dir.path().join("a-abc123.jpg");
        std::fs::write(&local_path, b"jpeg bytes").unwrap();

        push_one(
            &local_path,
            dest_dir.path().to_str().unwrap(),
            "a-abc123.jpg",
            &source_dir.path().join("push-logs"),
        )
        .await
        .unwrap();

        assert_eq!(
            std::fs::read(dest_dir.path().join("a-abc123.jpg")).unwrap(),
            b"jpeg bytes"
        );
    }
}
