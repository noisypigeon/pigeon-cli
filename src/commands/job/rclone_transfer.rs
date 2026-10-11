//! Shared `rclone copy` subprocess-spawn + live-JSON-log-polling mechanics
//! (ADR-0102), hoisted out of `rclone::worker::run_copy_job` once a second
//! real consumer needed the identical behavior (ADR-0112's `transform` job
//! pulls then pushes, two more call sites across one more job) -- this
//! codebase's usual "duplicate until the third consumer" precedent, counted
//! by call site rather than by job since all three sites run the exact same
//! subprocess/poll-loop code. `rclone::worker::run_copy_job` itself becomes a
//! thin wrapper around [`run_rclone_copy`] below, preserving its existing
//! behavior, metric labels, and tests exactly.

use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use tokio::io::AsyncReadExt;

use super::rclone_log::{RcloneLogSummary, RcloneLogTailer, TailDelta};

/// Same cadence `rclone::worker` used before the hoist (ADR-0102) --
/// independent of rclone's own fixed `--stats 30s` interval.
const LOG_POLL_INTERVAL: Duration = Duration::from_secs(10);

/// Emits this run's live metric deltas for one `poll()` of a copy tailer.
/// `job_name`/`phase_label` parameterize what used to be hardcoded
/// `"rclone-copy"`/`"transfer"` literals, so `transform`'s pull and push
/// phases get their own `pigeon_job_phase_total` identity instead of being
/// misreported as `rclone-copy`. `emit_upload_metrics` additionally gates
/// `pigeon_upload_bytes_total`/`pigeon_upload_outcomes_total`: `rclone copy`
/// (a single, one-directional invocation) and `transform`'s push phase both
/// want these; `transform`'s pull phase is a download, not an upload, and is
/// excluded.
fn emit_delta_metrics(
    job_name: &'static str,
    phase_label: &'static str,
    emit_upload_metrics: bool,
    delta: &TailDelta,
) {
    log_collapsed_repeats(phase_label, delta);

    if delta.is_empty() {
        return;
    }
    if delta.transferred > 0 {
        crate::observability::metrics::record_phase_count(
            job_name,
            phase_label,
            "transferred",
            delta.transferred,
            None,
        );
    }
    if delta.errors > 0 {
        crate::observability::metrics::record_phase_count(
            job_name,
            phase_label,
            "failed",
            delta.errors,
            None,
        );
    }
    if !emit_upload_metrics {
        return;
    }
    if delta.bytes > 0 {
        ::metrics::counter!(
            "pigeon_upload_bytes_total",
            "pigeon_job" => job_name,
            "instance" => crate::observability::instance(),
            "destination_bucket" => crate::observability::metrics::NO_BUCKET,
        )
        .increment(delta.bytes);
    }
    if delta.transferred > 0 {
        ::metrics::counter!(
            "pigeon_upload_outcomes_total",
            "pigeon_job" => job_name,
            "outcome" => "uploaded",
            "instance" => crate::observability::instance(),
            "destination_bucket" => crate::observability::metrics::NO_BUCKET,
        )
        .increment(delta.transferred);
    }
    if delta.errors > 0 {
        ::metrics::counter!(
            "pigeon_upload_outcomes_total",
            "pigeon_job" => job_name,
            "outcome" => "failed",
            "instance" => crate::observability::instance(),
            "destination_bucket" => crate::observability::metrics::NO_BUCKET,
        )
        .increment(delta.errors);
    }
}

/// Forwards each newly-copied object's relative path to `on_copied`, if a
/// caller opted in (ADR-0116). A send error means the receiver was dropped
/// -- the caller stopped listening, which is fine, not a failure of the
/// rclone subprocess itself.
fn forward_copied(
    on_copied: &Option<tokio::sync::mpsc::UnboundedSender<String>>,
    objects: Vec<String>,
) {
    if let Some(sender) = on_copied {
        for object in objects {
            let _ = sender.send(object);
        }
    }
}

/// Shared with `rclone::worker`'s own `delete`-side metrics emission --
/// logging a collapsed-repeats summary (ADR-0106) is identical regardless
/// of which rclone action produced it.
pub(crate) fn log_collapsed_repeats(step: &'static str, delta: &TailDelta) {
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
/// `tpslimit` resolved per-run (ADR-0108) and every other performance/retry
/// flag fixed, plus a structured JSON log redirected to `log_path`, polling
/// that log live while the subprocess runs (ADR-0102) so
/// `pigeon_job_phase_total`/`pigeon_upload_*` update mid-run instead of only
/// once at the end. `include_extension` (ADR-0112), when given, appends
/// `--include '*.<extension>' --ignore-case` to the rclone invocation --
/// `transform`'s pull phase uses this to avoid transferring anything outside
/// its single `--input-file-type` scope; `rclone copy` passes `None` (a copy
/// job has nothing to filter). Does not touch `pigeon_job_macro_phase` --
/// callers with a notion of "local work vs. uploading"
/// (`rclone::worker::run_copy_job`, `transform::worker`) set that gauge
/// themselves around this call, since whether a given invocation of this
/// function *is* "uploading" depends on which phase the caller is in, not on
/// this function's own logic.
///
/// `on_copied` (ADR-0116), when given, receives each object's relative path
/// the instant rclone's own JSON log reports it as newly copied -- the
/// authoritative, race-free "this file's bytes are fully on disk" signal.
/// `transform`'s pull phase uses this to pipeline transcode/push work off a
/// still-running bulk pull instead of waiting for the whole batch to finish;
/// every other caller (`rclone::worker::run_copy_job`) passes `None` and
/// sees no behavior change at all -- the tailer always populates
/// `TailDelta::copied_objects` regardless of whether anyone's listening.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn run_rclone_copy(
    source: &str,
    destination: &str,
    include_extension: Option<&str>,
    log_path: &Path,
    transfers: usize,
    checkers: usize,
    tpslimit: Option<usize>,
    job_name: &'static str,
    phase_label: &'static str,
    emit_upload_metrics: bool,
    on_copied: Option<tokio::sync::mpsc::UnboundedSender<String>>,
) -> Result<RcloneLogSummary, String> {
    tracing::info!(
        source,
        destination,
        phase = phase_label,
        "rclone: copy starting"
    );

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
    if let Some(extension) = include_extension {
        rclone_args.push("--include".to_string());
        rclone_args.push(format!("*.{extension}"));
        rclone_args.push("--ignore-case".to_string());
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
    // while the poll loop below awaits its exit.
    let mut stderr_pipe = child.stderr.take().expect("stderr was configured as piped");
    let stderr_task = tokio::spawn(async move {
        let mut buf = String::new();
        let _ = stderr_pipe.read_to_string(&mut buf).await;
        buf
    });

    let mut tailer = RcloneLogTailer::new(log_path, phase_label);
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
                let delta = tailer.poll();
                emit_delta_metrics(job_name, phase_label, emit_upload_metrics, &delta);
                forward_copied(&on_copied, delta.copied_objects);
            }
        }
    };
    let exit_status = exit_status.map_err(|err| format!("failed to run rclone: {err}"))?;

    // One last read to flush anything written between the final tick and
    // process exit.
    let final_delta = tailer.poll();
    emit_delta_metrics(job_name, phase_label, emit_upload_metrics, &final_delta);
    forward_copied(&on_copied, final_delta.copied_objects);
    let log_summary = tailer.summary();

    let exit_code = exit_status.code();
    let stderr = stderr_task.await.unwrap_or_default();

    tracing::info!(
        transferred = log_summary.transferred,
        errors = log_summary.errors,
        bytes = log_summary.bytes,
        exit_code,
        phase = phase_label,
        "rclone: copy complete"
    );

    if !exit_status.success() {
        return Err(format!(
            "rclone copy exited with status {} ({} file(s) transferred, {} error(s)); see {} for details{}",
            exit_code
                .map(|code| code.to_string())
                .unwrap_or_else(|| "unknown (terminated by signal)".to_string()),
            log_summary.transferred,
            log_summary.errors,
            log_path.display(),
            if stderr.trim().is_empty() {
                String::new()
            } else {
                format!(" -- rclone stderr: {}", stderr.trim())
            },
        ));
    }

    Ok(log_summary)
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

    /// Real end-to-end regression test against plain local filesystem paths,
    /// mirroring `rclone::worker`'s own pre-hoist test of identical shape.
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
        let summary = run_rclone_copy(
            source_dir.path().to_str().unwrap(),
            dest_dir.path().to_str().unwrap(),
            None,
            &log_path,
            2,
            4,
            None,
            "test-job",
            "transfer",
            true,
            None,
        )
        .await
        .unwrap();

        assert_eq!(summary.transferred, 1);
        assert_eq!(summary.errors, 0);
        assert!(dest_dir.path().join("hello.txt").exists());
    }

    /// The one behavior genuinely new vs. pre-hoist `run_copy_job`: an
    /// `include_extension` filter actually excludes a non-matching file
    /// rather than transferring everything found at the source.
    #[tokio::test]
    async fn include_extension_excludes_a_non_matching_file() {
        if check_rclone_available().await.is_err() {
            eprintln!("skipping: rclone not found on PATH");
            return;
        }

        let source_dir = tempfile::tempdir().unwrap();
        let dest_dir = tempfile::tempdir().unwrap();
        let log_dir = tempfile::tempdir().unwrap();
        std::fs::write(source_dir.path().join("photo.png"), b"png bytes").unwrap();
        std::fs::write(source_dir.path().join("notes.txt"), b"not a png").unwrap();

        let log_path = log_dir.path().join("rclone.jsonl");
        let summary = run_rclone_copy(
            source_dir.path().to_str().unwrap(),
            dest_dir.path().to_str().unwrap(),
            Some("png"),
            &log_path,
            2,
            4,
            None,
            "test-job",
            "pull",
            false,
            None,
        )
        .await
        .unwrap();

        assert_eq!(summary.transferred, 1);
        assert!(dest_dir.path().join("photo.png").exists());
        assert!(!dest_dir.path().join("notes.txt").exists());
    }

    /// `on_copied` (ADR-0116) receives each copied file's relative path
    /// exactly once -- the signal `transform`'s pull phase pipelines
    /// transcode work off, instead of waiting for the whole batch.
    #[tokio::test]
    async fn run_rclone_copy_sends_each_copied_objects_relative_path_on_the_channel() {
        if check_rclone_available().await.is_err() {
            eprintln!("skipping: rclone not found on PATH");
            return;
        }

        let source_dir = tempfile::tempdir().unwrap();
        let dest_dir = tempfile::tempdir().unwrap();
        let log_dir = tempfile::tempdir().unwrap();
        std::fs::write(source_dir.path().join("a.txt"), b"a").unwrap();
        std::fs::write(source_dir.path().join("b.txt"), b"b").unwrap();

        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let log_path = log_dir.path().join("rclone.jsonl");
        run_rclone_copy(
            source_dir.path().to_str().unwrap(),
            dest_dir.path().to_str().unwrap(),
            None,
            &log_path,
            2,
            4,
            None,
            "test-job",
            "transfer",
            true,
            Some(tx),
        )
        .await
        .unwrap();

        let mut received = Vec::new();
        while let Ok(object) = rx.try_recv() {
            received.push(object);
        }
        received.sort();
        assert_eq!(received, vec!["a.txt".to_string(), "b.txt".to_string()]);
    }
}
