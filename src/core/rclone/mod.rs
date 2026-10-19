//! Consolidates every `rclone` invocation this crate makes behind one
//! `RcloneCommand` trait and one shared runner (CLI-0132), sandboxed to a
//! config file pigeon itself generates from its own keyring, never an
//! externally-provisioned or user-general-purpose `rclone.conf` (CLI-0136).
//! Implementors declare what's genuinely their own (argv, log path,
//! observation mode, labels, how to turn a finished invocation into a
//! typed result); [`execute`] owns everything that was previously
//! duplicated across call sites: argv construction, the shared reliability
//! baseline, spawn, the concurrent stderr drain, the poll loop, and the
//! exit-status error text.

pub(crate) mod command;
pub(crate) mod config;
pub(crate) mod log;

use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use tokio::io::AsyncReadExt;

pub(crate) use log::{RcloneLogSummary, RcloneLogTailer, TailDelta};

/// Same cadence this crate has always used for live mid-run metrics
/// (CLI-0102) -- independent of rclone's own fixed `--stats 30s` interval.
const LOG_POLL_INTERVAL: Duration = Duration::from_secs(10);

/// A `Streamed` command's cumulative progress line fires at most once per
/// this many polls -- every 60s at `LOG_POLL_INTERVAL` (CLI-0156).
const PROGRESS_EVERY_POLLS: u32 = 6;

/// Per-object completion/PID channels a `Streamed` command may carry
/// (CLI-0116/CLI-0129). `Default`-empty for every command that doesn't need
/// them -- cheap to carry on every implementor via `..Default::default()`.
#[derive(Default, Clone)]
pub(crate) struct StreamHooks {
    pub on_copied: Option<tokio::sync::mpsc::UnboundedSender<String>>,
    pub pid_tx: Option<tokio::sync::mpsc::Sender<u32>>,
}

/// How a command's subprocess is driven: `Streamed` spawns, drains stderr
/// concurrently, and polls its `--log-file` live while the child runs
/// (`Copy`/`Purge`); `OneShot` runs to completion and inspects the final
/// exit status/stdout (`Lsjson`/`CopyTo`).
pub(crate) enum Observation {
    Streamed(StreamHooks),
    OneShot,
}

/// This invocation's metric/log identity.
pub(crate) struct Labels {
    /// `pigeon_job` label value, e.g. `"rclone-copy"`, `"transform"`.
    pub job: &'static str,
    /// `phase` metric label and log `step` field, e.g. `"transfer"`,
    /// `"pull"`, `"push"`, `"delete"`, `"list"`.
    pub step: &'static str,
    /// The `outcome` label a success carries.
    pub success_outcome: &'static str,
    /// Also emit `pigeon_upload_bytes_total`/`pigeon_upload_outcomes_total`
    /// from the live poll loop -- `Streamed` commands only (`Copy`'s own
    /// upload-direction invocations); `CopyTo` emits these itself in
    /// `finish`, since it has no live log to poll.
    pub upload_metrics: bool,
}

/// What a finished (or fully-drained) invocation produced, handed to
/// [`RcloneCommand::finish`] to turn into a typed result.
pub(crate) struct Outcome {
    pub exit_code: Option<i32>,
    pub success: bool,
    pub stdout: Vec<u8>,
    pub stderr: String,
    pub log: RcloneLogSummary,
}

/// Behavior shared by every `rclone` invocation this crate makes. Modeled on
/// `core::wizard::WizardInput`: implementors declare small descriptive
/// pieces, the trait's own provided method (`run`) holds the shared
/// algorithm.
pub(crate) trait RcloneCommand {
    type Output;

    /// `"copy"`, `"purge"`, `"lsjson"`, `"copyto"`.
    fn subcommand(&self) -> &'static str;
    /// Operands, then this command's own flags, in argv order. The shared
    /// reliability/observability baseline (`--retries`, `--config`, and,
    /// when [`log_path`](Self::log_path) is `Some`, `--use-json-log`/
    /// `--log-file`) is appended by [`execute`].
    fn args(&self) -> Vec<String>;
    /// Where this invocation's `--use-json-log` output goes, if it uses one
    /// at all -- `None` for `Lsjson`, whose own stdout *is* the data rclone
    /// returns, so it carries no separate operation log.
    fn log_path(&self) -> Option<&Path>;
    /// Streamed (spawn + live tail) or one-shot (`output()`).
    fn observe(&self) -> Observation;
    /// This invocation's metric labels and log `step` field.
    fn labels(&self) -> Labels;
    /// Turns a finished invocation into `Output` -- the one place a
    /// command-specific success judgment is allowed to live (e.g.
    /// `Lsjson`'s "directory not found" -> empty-listing tolerance).
    fn finish(&self, outcome: Outcome) -> Result<Self::Output, String>;
    /// `Some((attempts, backoff))` wraps the whole invocation in
    /// `core::retry::retry_with_backoff`; `None` relies on rclone's own
    /// `--retries`/`--low-level-retries`.
    fn retry(&self) -> Option<(usize, Duration)> {
        None
    }

    /// Overrides which config file this one invocation reads, instead of
    /// the keyring-derived one `config::rclone_config_path` names
    /// (CLI-0148 §3).
    ///
    /// Exists for exactly one case: probing a remote that is **not in the
    /// keyring yet**, which is what `keyring add bucket`'s verify-before-
    /// save does (CLI-0010). The generated config is a pure function of the
    /// saved keyring (CLI-0136), so a candidate bucket cannot appear in it
    /// without being saved first -- the ordering this breaks.
    ///
    /// Defaulted, like `retry`, so every ordinary invocation keeps reading
    /// the one generated config and none of the other implementors change.
    fn config_path(&self) -> Option<&Path> {
        None
    }

    /// Build argv, spawn, observe, emit tracing + metrics, then `finish`.
    async fn run(&self) -> Result<Self::Output, String>
    where
        Self: Sized,
    {
        match self.retry() {
            Some((attempts, backoff)) => {
                crate::core::retry::retry_with_backoff(attempts, backoff, || execute(self)).await
            }
            None => execute(self).await,
        }
    }
}

/// `rclone version`, checked once up front in every rclone-backed job's
/// wizard, before any prompts -- fails immediately on a missing binary
/// rather than partway through a long-running subprocess. Not itself an
/// `RcloneCommand`: it runs before any job identity or log path exists, so
/// it has no meaningful `Labels` and no `log_path`.
pub(crate) async fn check_available() -> Result<(), String> {
    tokio::process::Command::new("rclone")
        .arg("version")
        .output()
        .await
        .map_err(|_| "'rclone' was not found on PATH".to_string())?;
    Ok(())
}

/// Substrings in an rclone failure that mean the *destination endpoint as a
/// whole* was unreachable, as opposed to anything being wrong with the one
/// file being transferred (CLI-0144).
///
/// Deliberately a separate question from "does retrying help?" (the
/// distinction CLI-0143 drew between `is_non_retryable_transcode_error` and
/// `is_wrong_input_kind_error`): these errors *are* retryable, but they are
/// also *shared* -- every concurrent transfer against the same remote fails
/// identically -- which is what licenses handling them once for the whole
/// run instead of once per file.
///
/// These match rclone/Go's own text, not pigeon's, so unlike CLI-0143's two
/// guard consts they are genuinely version-fragile -- the same caution
/// `Lsjson`'s `"directory not found"` check carries (issue #63). Reconfirm
/// them against whatever rclone is actually deployed. The first entry is
/// verbatim from the real run that motivated this ADR: a 74-second DNS
/// blackout on the VPN tunnel carrying all of this host's DNS produced
/// `dial tcp: lookup s3.fr-par.scw.cloud: no such host` for all 275
/// concurrently-pushing files and nothing else.
pub(crate) fn is_endpoint_unreachable_error(err: &str) -> bool {
    const ENDPOINT_UNREACHABLE_SIGNATURES: &[&str] = &[
        "no such host",
        "dial tcp",
        "connection refused",
        "connection reset by peer",
        "i/o timeout",
        "no route to host",
        "network is unreachable",
        "TLS handshake timeout",
    ];
    ENDPOINT_UNREACHABLE_SIGNATURES
        .iter()
        .any(|signature| err.contains(signature))
}

async fn execute<C: RcloneCommand + ?Sized>(cmd: &C) -> Result<C::Output, String> {
    let mut argv = cmd.args();
    let streamed = matches!(cmd.observe(), Observation::Streamed(_));

    argv.extend(["--retries", "5", "--low-level-retries", "20"].map(String::from));
    if let Some(log_path) = cmd.log_path() {
        if let Some(parent) = log_path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|err| format!("failed to create {}: {err}", parent.display()))?;
        }
        if streamed {
            argv.extend(["--stats", "30s"].map(String::from));
        }
        argv.extend(["--use-json-log", "--log-level", "INFO", "--log-file"].map(String::from));
        argv.push(log_path.display().to_string());
    }
    argv.push("--config".to_string());
    argv.push(match cmd.config_path() {
        Some(path) => path.display().to_string(),
        None => config::rclone_config_path()?.display().to_string(),
    });

    let labels = cmd.labels();
    match cmd.observe() {
        Observation::Streamed(hooks) => execute_streamed(cmd, argv, &labels, hooks).await,
        Observation::OneShot => execute_one_shot(cmd, argv, &labels).await,
    }
}

async fn execute_streamed<C: RcloneCommand + ?Sized>(
    cmd: &C,
    argv: Vec<String>,
    labels: &Labels,
    hooks: StreamHooks,
) -> Result<C::Output, String> {
    let subcommand = cmd.subcommand();
    tracing::info!(step = labels.step, "rclone: {subcommand} starting");

    let mut child = tokio::process::Command::new("rclone")
        .arg(subcommand)
        .args(&argv)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|err| format!("failed to run rclone: {err}"))?;

    if let (Some(tx), Some(pid)) = (&hooks.pid_tx, child.id()) {
        let _ = tx.send(pid).await;
    }

    // Drained concurrently, not after `wait()`: a chatty subprocess could
    // otherwise block forever on a full stderr pipe nobody's reading from
    // while the poll loop below awaits its exit.
    let mut stderr_pipe = child.stderr.take().expect("stderr was configured as piped");
    let stderr_task = tokio::spawn(async move {
        let mut buf = String::new();
        let _ = stderr_pipe.read_to_string(&mut buf).await;
        buf
    });

    let log_path = cmd
        .log_path()
        .expect("a Streamed command always has a log_path")
        .to_path_buf();
    let mut tailer = RcloneLogTailer::new(&log_path, labels.step);
    let mut wait_handle = tokio::spawn(async move { child.wait().await });
    let mut poll_interval = tokio::time::interval(LOG_POLL_INTERVAL);
    // The first `tick()` fires immediately; that poll will almost always
    // find nothing yet (rclone hasn't opened its `--log-file`), which is
    // harmless -- `RcloneLogTailer::poll` treats a missing file as "no new
    // data," not an error.
    poll_interval.tick().await;
    // CLI-0156: a cumulative progress line every `PROGRESS_EVERY_POLLS`
    // polls, but only when the totals moved since the last one, so a
    // SIGSTOPped (CLI-0129) or idle transfer stays quiet. Until this, the
    // live figures computed every poll reached only Prometheus -- which is
    // best-effort, and absent for any run that lost the port race.
    let mut polls_since_progress: u32 = 0;
    let mut last_progress: Option<(u64, u64, u64)> = None;

    let exit_status = loop {
        tokio::select! {
            join_result = &mut wait_handle => {
                let wait_result = join_result
                    .map_err(|err| format!("rclone process join failed: {err}"))?;
                break wait_result;
            }
            _ = poll_interval.tick() => {
                let delta = tailer.poll();
                emit_delta_metrics(labels, &delta);
                forward_copied(&hooks.on_copied, delta.copied_objects);

                polls_since_progress += 1;
                if polls_since_progress >= PROGRESS_EVERY_POLLS {
                    polls_since_progress = 0;
                    let cumulative = tailer.summary();
                    let totals = (cumulative.transferred, cumulative.errors, cumulative.bytes);
                    if last_progress != Some(totals) {
                        last_progress = Some(totals);
                        tracing::info!(
                            transferred = cumulative.transferred,
                            errors = cumulative.errors,
                            bytes = cumulative.bytes,
                            step = labels.step,
                            "rclone: {subcommand} progress"
                        );
                    }
                }
            }
        }
    };
    let exit_status = exit_status.map_err(|err| format!("failed to run rclone: {err}"))?;

    // One last read to flush anything written between the final tick and
    // process exit.
    let final_delta = tailer.poll();
    emit_delta_metrics(labels, &final_delta);
    forward_copied(&hooks.on_copied, final_delta.copied_objects);
    let log_summary = tailer.summary();

    let exit_code = exit_status.code();
    let stderr = stderr_task.await.unwrap_or_default();

    tracing::info!(
        transferred = log_summary.transferred,
        errors = log_summary.errors,
        bytes = log_summary.bytes,
        deletes = log_summary.deletes,
        exit_code,
        step = labels.step,
        "rclone: {subcommand} complete"
    );

    cmd.finish(Outcome {
        exit_code,
        success: exit_status.success(),
        stdout: Vec::new(),
        stderr,
        log: log_summary,
    })
}

async fn execute_one_shot<C: RcloneCommand + ?Sized>(
    cmd: &C,
    argv: Vec<String>,
    labels: &Labels,
) -> Result<C::Output, String> {
    let output = tokio::process::Command::new("rclone")
        .arg(cmd.subcommand())
        .args(&argv)
        .output()
        .await
        .map_err(|err| format!("failed to run rclone: {err}"))?;

    let exit_code = output.status.code();
    let success = output.status.success();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();

    crate::observability::metrics::record_phase_count(
        labels.job,
        labels.step,
        if success {
            labels.success_outcome
        } else {
            "failed"
        },
        1,
        None,
    );

    cmd.finish(Outcome {
        exit_code,
        success,
        stdout: output.stdout,
        stderr,
        log: RcloneLogSummary::default(),
    })
}

/// Emits this run's live metric deltas for one `poll()` of a `Streamed`
/// tailer.
fn emit_delta_metrics(labels: &Labels, delta: &TailDelta) {
    log_collapsed_repeats(labels.step, delta);

    if delta.is_empty() {
        return;
    }
    if delta.transferred > 0 {
        crate::observability::metrics::record_phase_count(
            labels.job,
            labels.step,
            labels.success_outcome,
            delta.transferred,
            None,
        );
    }
    if delta.deletes > 0 {
        crate::observability::metrics::record_phase_count(
            labels.job,
            labels.step,
            labels.success_outcome,
            delta.deletes,
            None,
        );
    }
    if delta.errors > 0 {
        crate::observability::metrics::record_phase_count(
            labels.job,
            labels.step,
            "failed",
            delta.errors,
            None,
        );
    }
    if !labels.upload_metrics {
        return;
    }
    if delta.bytes > 0 {
        ::metrics::counter!(
            "pigeon_upload_bytes_total",
            "pigeon_job" => labels.job,
            "instance" => crate::observability::instance(),
            "destination_bucket" => crate::observability::metrics::NO_BUCKET,
        )
        .increment(delta.bytes);
    }
    if delta.transferred > 0 {
        ::metrics::counter!(
            "pigeon_upload_outcomes_total",
            "pigeon_job" => labels.job,
            "outcome" => "uploaded",
            "instance" => crate::observability::instance(),
            "destination_bucket" => crate::observability::metrics::NO_BUCKET,
        )
        .increment(delta.transferred);
    }
    if delta.errors > 0 {
        ::metrics::counter!(
            "pigeon_upload_outcomes_total",
            "pigeon_job" => labels.job,
            "outcome" => "failed",
            "instance" => crate::observability::instance(),
            "destination_bucket" => crate::observability::metrics::NO_BUCKET,
        )
        .increment(delta.errors);
    }
}

/// Forwards each newly-copied object's relative path to `on_copied`, if a
/// caller opted in (CLI-0116). A send error means the receiver was dropped
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

/// Logs a collapsed-repeats summary (CLI-0106) -- identical regardless of
/// which rclone action produced it.
fn log_collapsed_repeats(step: &'static str, delta: &TailDelta) {
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

#[cfg(test)]
mod tests {
    use super::*;

    /// The verbatim rclone error from the CLI-0144 run -- 275 of 275 failed
    /// pushes carried exactly this and nothing else.
    #[test]
    fn is_endpoint_unreachable_error_matches_the_real_dns_blackout_error() {
        assert!(is_endpoint_unreachable_error(
            "operation error S3: HeadObject, https response error StatusCode: 0, RequestID: , \
             HostID: , request send failed, Head \
             \"https://s3.fr-par.scw.cloud/pigeon-spc6f1-transform/jpg/a-abc123.jpg\": dial tcp: \
             lookup s3.fr-par.scw.cloud: no such host"
        ));
    }

    #[test]
    fn is_endpoint_unreachable_error_matches_the_other_connectivity_signatures() {
        for err in [
            "dial tcp 10.0.0.1:443: connect: connection refused",
            "read tcp 10.0.0.1:443: read: connection reset by peer",
            "Get \"https://example.invalid/\": net/http: TLS handshake timeout",
            "dial tcp: i/o timeout",
            "connect: no route to host",
            "connect: network is unreachable",
        ] {
            assert!(is_endpoint_unreachable_error(err), "{err}");
        }
    }

    /// A per-file fault must never be mistaken for the endpoint being down:
    /// that would park every other in-flight push behind a recovery wait
    /// that can never succeed.
    #[test]
    fn is_endpoint_unreachable_error_rejects_per_file_and_auth_failures() {
        for err in [
            "operation error S3: PutObject, https response error StatusCode: 403, \
             api error AccessDenied: Access Denied",
            "operation error S3: PutObject, https response error StatusCode: 503, \
             api error SlowDown: Please reduce your request rate",
            "failed to open source object: open /tmp/x.jpg: no such file or directory",
            "error listing: directory not found",
        ] {
            assert!(!is_endpoint_unreachable_error(err), "{err}");
        }
    }
}
