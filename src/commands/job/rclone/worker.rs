use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use tokio::io::AsyncReadExt;

use super::rclone_log::{self, RcloneLogTailer};

pub(crate) struct RcloneCopyPlan {
    pub source: String,
    pub destination: String,
    pub log_path: PathBuf,
    pub transfers: usize,
    pub checkers: usize,
    pub tpslimit: Option<usize>,
}

#[derive(Debug, Default, Clone)]
pub(crate) struct RcloneCopySummary {
    pub transferred: u64,
    pub errors: u64,
    pub bytes: u64,
    pub log_path: PathBuf,
}

pub(crate) struct RcloneDeletePlan {
    pub source: String,
    pub log_path: PathBuf,
    pub checkers: usize,
}

#[derive(Debug, Default, Clone)]
pub(crate) struct RcloneDeleteSummary {
    pub deleted: u64,
    pub errors: u64,
    pub log_path: PathBuf,
}

/// How often the still-running rclone subprocess's JSON log is re-read for
/// live metrics (ADR-0102) -- deliberately independent of rclone's own
/// fixed `--stats 30s` interval (which governs how often a cumulative-totals
/// line actually appears in the log), just frequent enough that a per-object
/// error line surfaces as a `tracing::warn!` close to when it happened,
/// rather than only once the whole subprocess exits.
const LOG_POLL_INTERVAL: Duration = Duration::from_secs(10);

/// Confirms `rclone` is on `PATH`, checked once up front in the wizard
/// before any prompts (mirrors `pull_transform::media::check_ffmpeg_available`)
/// so a missing binary fails immediately, not partway through a
/// long-running `rclone copy`/`rclone purge` subprocess.
pub(crate) async fn check_rclone_available() -> Result<(), String> {
    tokio::process::Command::new("rclone")
        .arg("version")
        .output()
        .await
        .map_err(|_| {
            "'rclone' was not found on PATH -- required for 'pigeon job run rclone' to copy/delete data"
                .to_string()
        })?;
    Ok(())
}

/// Emits this run's live metric deltas for one `poll()` of a `copy` tailer --
/// shared by the in-progress polling loop and the final post-exit flush, so
/// both paths report identically.
fn emit_delta_metrics_copy(delta: &rclone_log::TailDelta) {
    log_collapsed_repeats("transfer", delta);

    if delta.is_empty() {
        return;
    }
    if delta.transferred > 0 {
        crate::observability::metrics::record_phase_count(
            "rclone-copy",
            "transfer",
            "transferred",
            delta.transferred,
            None,
        );
    }
    if delta.errors > 0 {
        crate::observability::metrics::record_phase_count(
            "rclone-copy",
            "transfer",
            "failed",
            delta.errors,
            None,
        );
    }
    if delta.bytes > 0 {
        ::metrics::counter!(
            "pigeon_upload_bytes_total",
            "pigeon_job" => "rclone-copy",
            "instance" => crate::observability::instance(),
            "destination_bucket" => crate::observability::metrics::NO_BUCKET,
        )
        .increment(delta.bytes);
    }
    if delta.transferred > 0 {
        ::metrics::counter!(
            "pigeon_upload_outcomes_total",
            "pigeon_job" => "rclone-copy",
            "outcome" => "uploaded",
            "instance" => crate::observability::instance(),
            "destination_bucket" => crate::observability::metrics::NO_BUCKET,
        )
        .increment(delta.transferred);
    }
    if delta.errors > 0 {
        ::metrics::counter!(
            "pigeon_upload_outcomes_total",
            "pigeon_job" => "rclone-copy",
            "outcome" => "failed",
            "instance" => crate::observability::instance(),
            "destination_bucket" => crate::observability::metrics::NO_BUCKET,
        )
        .increment(delta.errors);
    }
}

/// Emits this run's live metric deltas for one `poll()` of a `delete`
/// tailer. Unlike `emit_delta_metrics_copy`, never emits
/// `pigeon_upload_bytes_total`/`pigeon_upload_outcomes_total` -- purge
/// moves no bytes and isn't an "upload" in the sense those two metrics
/// model.
fn emit_delta_metrics_delete(delta: &rclone_log::TailDelta) {
    log_collapsed_repeats("delete", delta);

    if delta.is_empty() {
        return;
    }
    if delta.deletes > 0 {
        crate::observability::metrics::record_phase_count(
            "rclone-delete",
            "delete",
            "deleted",
            delta.deletes,
            None,
        );
    }
    if delta.errors > 0 {
        crate::observability::metrics::record_phase_count(
            "rclone-delete",
            "delete",
            "failed",
            delta.errors,
            None,
        );
    }
}

/// Logged regardless of `delta.is_empty()` -- a collapsed-repeats-only poll
/// (no new transfer/delete/error/byte delta) still has something worth
/// reporting (ADR-0106); the two are independent concerns.
fn log_collapsed_repeats(step: &'static str, delta: &rclone_log::TailDelta) {
    for summary in &delta.collapsed_repeats {
        tracing::warn!(
            cause = %summary.cause,
            first_key = ?summary.first_key,
            repeated = summary.repeated,
            step = step,
            "rclone: {} more object failure(s) with the same cause since the last report",
            summary.repeated
        );
    }
}

/// Runs `rclone copy <source> <destination>` with `transfers`/`checkers`/
/// `tpslimit` resolved per-run (ADR-0108, overriding ADR-0106's prior
/// fixed `8`/`16`/`10` values -- a Scaleway-backed transfer showed those
/// fixed values too conservative for every destination) and every other
/// performance/retry flag still fixed, plus a structured JSON log
/// redirected to `log_path`, polling that log live while the subprocess
/// runs (ADR-0102) so `pigeon_job_phase_total`/`pigeon_upload_*` update
/// mid-run instead of only once at the end.
pub(crate) async fn run_copy_job(
    source: &str,
    destination: &str,
    log_path: &Path,
    transfers: usize,
    checkers: usize,
    tpslimit: Option<usize>,
) -> Result<RcloneCopySummary, String> {
    tracing::info!(source, destination, "rclone: copy starting");

    if let Some(parent) = log_path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|err| format!("failed to create {}: {err}", parent.display()))?;
    }

    let mut rclone_args: Vec<String> = vec![
        "--transfers".to_string(),
        transfers.to_string(),
        "--checkers".to_string(),
        checkers.to_string(),
    ];
    if let Some(tpslimit) = tpslimit {
        rclone_args.push("--tpslimit".to_string());
        rclone_args.push(tpslimit.to_string());
    }
    rclone_args.extend(
        [
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
        ]
        .iter()
        .map(|s| s.to_string()),
    );

    let mut child = tokio::process::Command::new("rclone")
        .arg("copy")
        .arg(source)
        .arg(destination)
        .args(rclone_args)
        .arg(log_path)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|err| format!("failed to run rclone: {err}"))?;

    // Drained concurrently, not after `wait()`: a chatty subprocess could
    // otherwise block forever on a full stderr pipe nobody's reading from
    // while the poll loop below awaits its exit (`.output()`, used before
    // ADR-0102 needed a live poll loop alongside it, drained this for free).
    let mut stderr_pipe = child.stderr.take().expect("stderr was configured as piped");
    let stderr_task = tokio::spawn(async move {
        let mut buf = String::new();
        let _ = stderr_pipe.read_to_string(&mut buf).await;
        buf
    });

    // Copy has no separate "local work" phase the way download/hash/
    // placement jobs do (ADR-0093's gauge was designed for that split) --
    // its entire body of work *is* the transfer, so it goes straight to
    // "uploading" rather than following the false/then/true pattern every
    // other job's `run_<job>_job` + `upload.rs` pairing uses.
    crate::observability::metrics::set_macro_phase("rclone-copy", true);

    let mut tailer = RcloneLogTailer::new(log_path, "transfer");
    let mut wait_handle = tokio::spawn(async move { child.wait().await });
    let mut poll_interval = tokio::time::interval(LOG_POLL_INTERVAL);
    // The first `tick()` fires immediately; that poll will almost always
    // find nothing yet (rclone hasn't opened its `--log-file`), which is
    // harmless -- `RcloneLogTailer::poll` treats a missing file as "no new
    // data," not an error.
    poll_interval.tick().await;

    let exit_status = loop {
        tokio::select! {
            join_result = &mut wait_handle => {
                let wait_result = join_result
                    .map_err(|err| format!("rclone process join failed: {err}"))?;
                break wait_result;
            }
            _ = poll_interval.tick() => {
                emit_delta_metrics_copy(&tailer.poll());
            }
        }
    };
    let exit_status = exit_status.map_err(|err| format!("failed to run rclone: {err}"))?;

    // One last read to flush anything written between the final tick and
    // process exit.
    emit_delta_metrics_copy(&tailer.poll());
    let log_summary = tailer.summary();

    let exit_code = exit_status.code();
    let stderr = stderr_task.await.unwrap_or_default();

    tracing::info!(
        transferred = log_summary.transferred,
        errors = log_summary.errors,
        bytes = log_summary.bytes,
        exit_code,
        "rclone: copy complete"
    );

    let summary = RcloneCopySummary {
        transferred: log_summary.transferred,
        errors: log_summary.errors,
        bytes: log_summary.bytes,
        log_path: log_path.to_path_buf(),
    };

    if !exit_status.success() {
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

/// Runs `rclone purge <source>` (ADR-0110) -- a recursive, irreversible
/// delete of everything under `source`. `--checkers` is still meaningful
/// (purge enumerates objects before deleting them) and `--fast-list` is
/// kept for the same reason (a listing optimization, not a transfer one).
/// `--transfers`/`--tpslimit` don't exist on this path at all -- no file
/// content moves, so no transfer concurrency or rate concept applies.
pub(crate) async fn run_delete_job(
    source: &str,
    log_path: &Path,
    checkers: usize,
) -> Result<RcloneDeleteSummary, String> {
    tracing::info!(source, "rclone: purge starting");

    if let Some(parent) = log_path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|err| format!("failed to create {}: {err}", parent.display()))?;
    }

    let rclone_args: Vec<String> = [
        "--checkers",
        &checkers.to_string(),
        "--fast-list",
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
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();

    let mut child = tokio::process::Command::new("rclone")
        .arg("purge")
        .arg(source)
        .args(rclone_args)
        .arg(log_path)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|err| format!("failed to run rclone: {err}"))?;

    let mut stderr_pipe = child.stderr.take().expect("stderr was configured as piped");
    let stderr_task = tokio::spawn(async move {
        let mut buf = String::new();
        let _ = stderr_pipe.read_to_string(&mut buf).await;
        buf
    });

    // Delete has no separate "local work" phase either -- same reasoning as
    // copy, just for enumeration+deletion instead of transfer.
    crate::observability::metrics::set_macro_phase("rclone-delete", true);

    let mut tailer = RcloneLogTailer::new(log_path, "delete");
    let mut wait_handle = tokio::spawn(async move { child.wait().await });
    let mut poll_interval = tokio::time::interval(LOG_POLL_INTERVAL);
    poll_interval.tick().await;

    let exit_status = loop {
        tokio::select! {
            join_result = &mut wait_handle => {
                let wait_result = join_result
                    .map_err(|err| format!("rclone process join failed: {err}"))?;
                break wait_result;
            }
            _ = poll_interval.tick() => {
                emit_delta_metrics_delete(&tailer.poll());
            }
        }
    };
    let exit_status = exit_status.map_err(|err| format!("failed to run rclone: {err}"))?;

    emit_delta_metrics_delete(&tailer.poll());
    let log_summary = tailer.summary();

    let exit_code = exit_status.code();
    let stderr = stderr_task.await.unwrap_or_default();

    tracing::info!(
        deleted = log_summary.deletes,
        errors = log_summary.errors,
        exit_code,
        "rclone: purge complete"
    );

    let summary = RcloneDeleteSummary {
        deleted: log_summary.deletes,
        errors: log_summary.errors,
        log_path: log_path.to_path_buf(),
    };

    if !exit_status.success() {
        return Err(format!(
            "rclone purge exited with status {} ({} object(s) deleted, {} error(s)); see {} for details{}",
            exit_code
                .map(|code| code.to_string())
                .unwrap_or_else(|| "unknown (terminated by signal)".to_string()),
            summary.deleted,
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
        let summary = run_copy_job(
            source_dir.path().to_str().unwrap(),
            dest_dir.path().to_str().unwrap(),
            &log_path,
            2,
            4,
            None,
        )
        .await
        .unwrap();

        assert_eq!(summary.transferred, 1);
        assert_eq!(summary.errors, 0);
        assert!(dest_dir.path().join("hello.txt").exists());
    }

    /// A `Some(tpslimit)` must not break the rclone invocation -- the
    /// ADR-0106 default this ADR-0108 reverted is still a valid value to
    /// opt back into per-destination.
    #[tokio::test]
    async fn copies_a_file_with_a_tpslimit_set() {
        if check_rclone_available().await.is_err() {
            eprintln!("skipping: rclone not found on PATH");
            return;
        }

        let source_dir = tempfile::tempdir().unwrap();
        let dest_dir = tempfile::tempdir().unwrap();
        let log_dir = tempfile::tempdir().unwrap();
        std::fs::write(source_dir.path().join("hello.txt"), b"hello world").unwrap();

        let log_path = log_dir.path().join("rclone.jsonl");
        let summary = run_copy_job(
            source_dir.path().to_str().unwrap(),
            dest_dir.path().to_str().unwrap(),
            &log_path,
            8,
            16,
            Some(10),
        )
        .await
        .unwrap();

        assert_eq!(summary.transferred, 1);
        assert_eq!(summary.errors, 0);
        assert!(dest_dir.path().join("hello.txt").exists());
    }

    /// Real end-to-end `rclone purge` against a local directory tree with
    /// zero `rclone.conf` needed, mirroring the copy tests above. Confirms
    /// purge actually removes both files and the directory structure
    /// itself (unlike rclone's own `delete` subcommand, which leaves empty
    /// directories behind).
    #[tokio::test]
    async fn purges_everything_under_a_local_directory() {
        if check_rclone_available().await.is_err() {
            eprintln!("skipping: rclone not found on PATH");
            return;
        }

        let source_dir = tempfile::tempdir().unwrap();
        let log_dir = tempfile::tempdir().unwrap();
        std::fs::write(source_dir.path().join("a.txt"), b"a").unwrap();
        std::fs::create_dir(source_dir.path().join("sub")).unwrap();
        std::fs::write(source_dir.path().join("sub/b.txt"), b"b").unwrap();

        let log_path = log_dir.path().join("rclone-purge.jsonl");
        let summary = run_delete_job(source_dir.path().to_str().unwrap(), &log_path, 4)
            .await
            .unwrap();

        assert_eq!(summary.errors, 0);
        assert!(summary.deleted >= 1);
        assert!(!source_dir.path().join("a.txt").exists());
        assert!(!source_dir.path().join("sub").exists());
    }
}
