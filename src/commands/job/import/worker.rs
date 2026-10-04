use std::path::{Path, PathBuf};

use super::rclone_log;

pub(crate) struct ImportPlan {
    pub source: String,
    pub destination: String,
    pub log_path: PathBuf,
}

#[derive(Debug, Default, Clone)]
pub(crate) struct ImportSummary {
    pub transferred: u64,
    pub errors: u64,
    pub bytes: u64,
    pub log_path: PathBuf,
}

/// Confirms `rclone` is on `PATH`, checked once up front in the wizard
/// before any prompts (mirrors `pull_transform::media::check_ffmpeg_available`)
/// so a missing binary fails immediately, not partway through a
/// long-running `rclone copy` subprocess.
pub(crate) async fn check_rclone_available() -> Result<(), String> {
    tokio::process::Command::new("rclone")
        .arg("version")
        .output()
        .await
        .map_err(|_| {
            "'rclone' was not found on PATH -- required for 'pigeon job run import' to copy data"
                .to_string()
        })?;
    Ok(())
}

/// Runs `rclone copy <source> <destination>` with a fixed set of
/// performance/retry flags (ADR-0101 -- not configurable per run) and a
/// structured JSON log redirected to `log_path`, then parses that log for
/// this run's counts and per-object errors.
pub(crate) async fn run_import_job(
    source: &str,
    destination: &str,
    log_path: &Path,
) -> Result<ImportSummary, String> {
    tracing::info!(source, destination, "import: rclone copy starting");

    if let Some(parent) = log_path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|err| format!("failed to create {}: {err}", parent.display()))?;
    }

    let output = tokio::process::Command::new("rclone")
        .arg("copy")
        .arg(source)
        .arg(destination)
        .args([
            "--transfers",
            "32",
            "--checkers",
            "64",
            "--fast-list",
            "--buffer-size",
            "32M",
            "--multi-thread-streams",
            "4",
            "--multi-thread-cutoff",
            "256M",
            "--retries",
            "5",
            "--low-level-retries",
            "20",
            "--stats",
            "30s",
            "--use-json-log",
            "--log-level",
            "INFO",
            "--log-file",
        ])
        .arg(log_path)
        .output()
        .await
        .map_err(|err| format!("failed to run rclone: {err}"))?;

    let exit_code = output.status.code();

    // Parse whatever got written to the log regardless of exit status -- a
    // run that hit a transfer/duration limit or died partway through still
    // transferred real files worth counting and reporting.
    let log_summary = rclone_log::parse_and_report(log_path).unwrap_or_else(|err| {
        tracing::warn!(error = %err, "failed to parse rclone log file");
        rclone_log::RcloneLogSummary::default()
    });

    crate::observability::metrics::record_phase_count(
        "import",
        "transfer",
        "transferred",
        log_summary.transferred,
        None,
    );
    crate::observability::metrics::record_phase_count(
        "import",
        "transfer",
        "failed",
        log_summary.errors,
        None,
    );

    tracing::info!(
        transferred = log_summary.transferred,
        errors = log_summary.errors,
        bytes = log_summary.bytes,
        exit_code,
        "import: rclone copy complete"
    );

    let summary = ImportSummary {
        transferred: log_summary.transferred,
        errors: log_summary.errors,
        bytes: log_summary.bytes,
        log_path: log_path.to_path_buf(),
    };

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!(
            "rclone copy exited with status {} ({} file(s) transferred, {} error(s)); see {} for details{}",
            exit_code
                .map(|code| code.to_string())
                .unwrap_or_else(|| "unknown (terminated by signal)".to_string()),
            summary.transferred,
            summary.errors,
            log_path.display(),
            if stderr.trim().is_empty() {
                String::new()
            } else {
                format!(" -- rclone stderr: {}", stderr.trim())
            },
        ));
    }

    Ok(summary)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `rclone copy` works against plain local filesystem paths with zero
    /// `rclone.conf` needed, so this is a real end-to-end regression test,
    /// not just a parsing/heuristic check. Skipped (not failed) if `rclone`
    /// genuinely isn't on `PATH`, since this whole job already refuses to
    /// run without it (`check_rclone_available`) -- this test would just be
    /// redundant with that failure on a machine that can't run it anyway.
    #[tokio::test]
    async fn copies_a_file_between_two_local_directories() {
        if check_rclone_available().await.is_err() {
            eprintln!("skipping: rclone not found on PATH");
            return;
        }

        let source_dir = tempfile::tempdir().unwrap();
        let dest_dir = tempfile::tempdir().unwrap();
        let log_dir = tempfile::tempdir().unwrap();
        std::fs::write(source_dir.path().join("hello.txt"), b"hello world").unwrap();

        let log_path = log_dir.path().join("rclone.jsonl");
        let summary = run_import_job(
            source_dir.path().to_str().unwrap(),
            dest_dir.path().to_str().unwrap(),
            &log_path,
        )
        .await
        .unwrap();

        assert_eq!(summary.transferred, 1);
        assert_eq!(summary.errors, 0);
        assert!(dest_dir.path().join("hello.txt").exists());
    }
}
