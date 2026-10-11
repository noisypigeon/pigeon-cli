//! Per-file push via `rclone copyto` (ADR-0116) -- replaces the old bulk
//! Phase C `rclone copy` of the whole `result/` tree. `copyto <src-file>
//! <dest-file>` is rclone's documented single-file-to-single-file-path
//! primitive: no directory listing round trip for one named object, unlike
//! the bulk-tree `copy` the pull phase still uses. Pushing immediately after
//! each file transcodes (rather than waiting for the whole batch) is the
//! other half of the fix for a single bad file silently discarding every
//! already-succeeded file's push (ADR-0116's Context).

use std::path::Path;
use std::time::Duration;

use crate::core::retry::retry_with_backoff;

/// Wraps the whole `rclone copyto` subprocess invocation in one more outer
/// retry layer, same shape and values as `upload.rs`'s
/// `UPLOAD_RETRIES`/`UPLOAD_RETRY_BACKOFF` -- `copyto` itself still carries
/// rclone's own `--retries 5 --low-level-retries 20` internally; this layer
/// catches connection-level failures that occur before rclone's own retry
/// logic can even engage (e.g. the subprocess failing to spawn, or a
/// destination that's transiently unreachable at listing time).
const PUSH_RETRIES: usize = 3;
const PUSH_RETRY_BACKOFF: Duration = Duration::from_secs(2);

/// Pushes one already-placed file to `<destination_path>/<destination_filename>`.
/// `push_log_dir` holds one small JSON log per file (never one shared log
/// path -- many concurrent single-file `rclone` subprocesses cannot safely
/// share one `--log-file`), purely for diagnostics on failure.
pub(crate) async fn push_one(
    local_path: &Path,
    destination_path: &str,
    destination_filename: &str,
    push_log_dir: &Path,
) -> Result<(), String> {
    let destination = format!(
        "{}/{destination_filename}",
        destination_path.trim_end_matches('/')
    );
    let log_path = push_log_dir.join(format!("{destination_filename}.jsonl"));

    let result = retry_with_backoff(PUSH_RETRIES, PUSH_RETRY_BACKOFF, || {
        run_copyto(local_path, &destination, &log_path)
    })
    .await;

    crate::observability::metrics::record_phase_count(
        "transform",
        "push",
        if result.is_ok() {
            "transferred"
        } else {
            "failed"
        },
        1,
        None,
    );

    result
}

async fn run_copyto(local_path: &Path, destination: &str, log_path: &Path) -> Result<(), String> {
    if let Some(parent) = log_path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|err| format!("failed to create {}: {err}", parent.display()))?;
    }

    let output = tokio::process::Command::new("rclone")
        .arg("copyto")
        .arg(local_path)
        .arg(destination)
        .args([
            "--retries",
            "5",
            "--low-level-retries",
            "20",
            "--use-json-log",
            "--log-level",
            "INFO",
            "--log-file",
        ])
        .arg(log_path)
        .output()
        .await
        .map_err(|err| format!("failed to run rclone copyto: {err}"))?;

    if !output.status.success() {
        return Err(format!(
            "rclone copyto {} -> {destination} exited with status {}; see {} for details{}",
            local_path.display(),
            output
                .status
                .code()
                .map(|code| code.to_string())
                .unwrap_or_else(|| "unknown (terminated by signal)".to_string()),
            log_path.display(),
            if output.stderr.is_empty() {
                String::new()
            } else {
                format!(
                    " -- rclone stderr: {}",
                    String::from_utf8_lossy(&output.stderr).trim()
                )
            },
        ));
    }
    Ok(())
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
