//! Shared "upload this run's report/log/transcript to a bucket" logic
//! (ADR-0100), used identically by every job wizard's `dispatch_async`: a
//! mandatory `ReportBucketInput` (`shared_wizard.rs`) resolves a
//! bucket-config, and this module generates a per-run identifier, writes a
//! generic `Debug`-based report for jobs that don't already have their own
//! (ADR-0082's `deduplicate` keeps its bespoke one instead), and uploads the
//! report, the shared `pigeon.jsonl` log, and the run's transcript
//! (`observability::transcript::Transcript`) under one dated prefix.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::commands::job::shared_wizard::ReportBucketInput;
use crate::commands::keyring::bucket::store::BucketConfig;
use crate::commands::keyring::store::Store;
use crate::core::rclone::RcloneCommand as _;
use crate::core::rclone::command::CopyTo;
use crate::core::rclone::config::RemoteTarget;
use crate::core::wizard::WizardInput;
use crate::observability::{self, transcript::Transcript};

/// Resolves `--report-bucket` (mandatory, ADR-0100) and fetches its
/// `BucketConfig`/secret -- the same three-step lookup every job's other
/// bucket-config inputs already do (e.g. `SourceBucketInput`'s call sites).
pub(crate) fn resolve(
    flag: Option<String>,
    store: &Store,
    non_interactive: bool,
) -> Result<BucketConfig, String> {
    let alias = (ReportBucketInput { flag, store }).resolve(non_interactive)?;
    let bucket_config = store
        .bucket_configs()
        .find(|bucket_config| bucket_config.alias == alias)
        .ok_or_else(|| format!("no bucket-config named '{alias}'"))?
        .clone();
    Ok(bucket_config)
}

/// A collision-resistant-enough per-run identifier, deliberately with no new
/// crate dependency (this codebase has no `uuid`/`rand` dependency) -- a
/// nanosecond timestamp, this process's id, and the cached instance hostname
/// (ADR-0097, disambiguating across hosts), hex/string-joined. Not
/// cryptographically unique, just unique enough for a path segment.
pub(crate) fn generate_run_id() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    format!(
        "{nanos:x}-{}-{:x}",
        observability::instance(),
        std::process::id()
    )
}

/// `{YYYY-MM-DD}-{job_name}-{run_id}` -- the destination prefix a run's
/// report/log/transcript upload lands under (ADR-0100).
pub(crate) fn run_prefix(job_name: &str, run_id: &str) -> String {
    let now = time::OffsetDateTime::now_utc();
    format!(
        "{:04}-{:02}-{:02}-{job_name}-{run_id}",
        now.year(),
        u8::from(now.month()),
        now.day()
    )
}

/// Writes `<job_name>-report.txt` under `local_output` by `Debug`-formatting
/// `summary` -- the generic report for the job types that don't have a
/// bespoke one of their own (ADR-0100). Used both for a successful run's
/// `*Summary` struct and, on failure, for the error message itself, so a
/// report always exists to upload either way.
pub(crate) fn write_summary_report(
    local_output: &Path,
    job_name: &str,
    summary: &impl std::fmt::Debug,
) -> Result<PathBuf, String> {
    let path = local_output.join(format!("{job_name}-report.txt"));
    fs::write(&path, format!("{summary:#?}\n"))
        .map_err(|err| format!("failed to write {}: {err}", path.display()))?;
    Ok(path)
}

/// Creates this run's transcript file at `local_output/transcript.txt`,
/// returning both the recorder and its path (the latter needed later to
/// upload it).
pub(crate) fn new_transcript(local_output: &Path) -> Result<(Transcript, PathBuf), String> {
    let path = local_output.join("transcript.txt");
    let transcript = Transcript::create(&path)?;
    Ok((transcript, path))
}

/// Prints `message` exactly like a bare `println!` always has, while also
/// recording it to this run's transcript (ADR-0100).
pub(crate) fn say(transcript: &Transcript, message: impl AsRef<str>) {
    let message = message.as_ref();
    println!("{message}");
    transcript.line(message);
}

/// Records "Error: {err}" to both stdout and the transcript (ADR-0105) --
/// call immediately before `fail(err)` at any call site where a
/// `Transcript` is already in scope, so the same failure message that
/// reaches stderr/pigeon.jsonl also lands in the archived transcript.txt
/// instead of leaving it empty.
pub(crate) fn say_error(transcript: &Transcript, err: impl std::fmt::Display) {
    say(transcript, format!("Error: {err}"));
}

/// Logs this run's own outcome (ADR-0104) immediately before
/// `upload_run_artifacts` runs, from inside the still-open `command` span --
/// so the uploaded `pigeon.jsonl` snapshot always contains at least one line
/// stating how the run ended, instead of relying solely on
/// `run_instrumented`'s own "command finished" line, which is written only
/// *after* the whole dispatch (including this artifact upload) returns --
/// too late to ever appear in the snapshot it describes. Additive:
/// `run_instrumented`'s line is unchanged and still the authoritative one
/// for the live/ambient log.
pub(crate) fn log_run_outcome(exit_code: i32) {
    if exit_code == 0 {
        tracing::info!(exit_code, "job run outcome before artifact upload: success");
    } else {
        tracing::error!(exit_code, "job run outcome before artifact upload: failure");
    }
}

/// Uploads `report_path`, the shared `pigeon.jsonl` log, and
/// `transcript_path` to `bucket_config` under `{prefix}/`. Best-effort: a
/// failure here is `tracing::warn!`-logged but never changes the calling
/// job's own exit code (ADR-0100) -- this is instrumentation about the run,
/// not the run's primary deliverable.
/// Takes no secret: since ADR-0148 these go through `rclone copyto`,
/// which reads credentials from the generated config (and, since that
/// ADR's §1, honours `PIGEON_SECRET_<ALIAS>` while doing so).
pub(crate) async fn upload_run_artifacts(
    bucket_config: &BucketConfig,
    prefix: &str,
    report_path: &Path,
    transcript_path: &Path,
) {
    let destination = RemoteTarget::bucket_root(&bucket_config.alias).address();
    let log_dir = report_path.parent().unwrap_or(Path::new("."));

    let uploads: [(&str, &Path); 2] = [
        ("report.txt", report_path),
        ("transcript.txt", transcript_path),
    ];
    for (name, path) in uploads {
        upload_one(&destination, &format!("{prefix}/{name}"), path, log_dir).await;
    }
    match observability::log_file_path() {
        Ok(log_path) => {
            upload_one(
                &destination,
                &format!("{prefix}/pigeon.jsonl"),
                &log_path,
                log_dir,
            )
            .await;
        }
        Err(err) => {
            tracing::warn!(error = %err, "failed to resolve log file path for run artifact upload");
        }
    }
}

/// One artifact, via `rclone copyto`. `emit_upload_metrics` is deliberately
/// `false`: these three files are instrumentation *about* a run, and
/// counting them in `pigeon_upload_bytes_total` alongside the run's real
/// payload would inflate every job's transferred-bytes series by a
/// constant that has nothing to do with the data it moved.
///
/// Each gets its own rclone log, named after the flattened key -- many
/// single-file `rclone` invocations cannot safely share one `--log-file`
/// (ADR-0116), and these three run back to back.
async fn upload_one(destination: &str, key: &str, path: &Path, log_dir: &Path) {
    let flattened: String = key
        .chars()
        .map(|ch| if ch == '/' { '-' } else { ch })
        .collect();
    let result = (CopyTo {
        source: path.display().to_string(),
        destination: format!("{destination}{key}"),
        log_path: log_dir.join(format!("artifact-{flattened}.jsonl")),
        job: "report-upload",
        step: "artifact",
        emit_upload_metrics: false,
        bytes: 0,
    })
    .run()
    .await;
    if let Err(err) = result {
        tracing::warn!(key, error = %err, "failed to upload run artifact");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generate_run_id_is_not_empty_and_varies_across_calls() {
        let first = generate_run_id();
        let second = generate_run_id();
        assert!(!first.is_empty());
        assert_ne!(first, second);
    }

    #[test]
    fn run_prefix_has_the_expected_shape() {
        let prefix = run_prefix("deduplicate", "abc123");
        assert!(prefix.ends_with("-deduplicate-abc123"));
        assert_eq!(
            prefix.len(),
            "YYYY-MM-DD".len() + "-deduplicate-abc123".len()
        );
    }

    #[test]
    fn write_summary_report_debug_formats_the_summary() {
        #[derive(Debug)]
        #[allow(dead_code)]
        struct Summary {
            processed: usize,
        }
        let dir = tempfile::tempdir().unwrap();
        let path =
            write_summary_report(dir.path(), "deduplicate", &Summary { processed: 3 }).unwrap();
        let contents = fs::read_to_string(&path).unwrap();
        assert!(contents.contains("processed: 3"));
        assert_eq!(path.file_name().unwrap(), "deduplicate-report.txt");
    }

    #[test]
    fn write_summary_report_also_works_for_an_error_string() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_summary_report(dir.path(), "transform", &"boom".to_string()).unwrap();
        let contents = fs::read_to_string(&path).unwrap();
        assert!(contents.contains("boom"));
    }

    #[test]
    fn say_error_prefixes_and_records_the_message() {
        let dir = tempfile::tempdir().unwrap();
        let (transcript, path) = new_transcript(dir.path()).unwrap();
        say_error(&transcript, "boom");
        assert_eq!(fs::read_to_string(&path).unwrap(), "Error: boom\n");
    }

    /// A minimal `tracing_subscriber::Layer` that captures every event's
    /// fields into a plain map, for asserting on `log_run_outcome`'s
    /// (ADR-0104) `tracing::info!`/`error!` calls without needing a global
    /// subscriber or an extra test-only crate dependency. Mirrors the
    /// `CapturedEvents`/`CaptureLayer` pattern already established in
    /// the since-removed `email_sync/transform.rs`'s test module
    /// (ADR-0080); duplicated here
    /// per this codebase's "duplicate until the third consumer" precedent.
    #[derive(Clone, Default)]
    struct CapturedEvents(
        std::sync::Arc<std::sync::Mutex<Vec<std::collections::HashMap<String, String>>>>,
    );

    struct CaptureLayer(CapturedEvents);

    impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for CaptureLayer {
        fn on_event(
            &self,
            event: &tracing::Event<'_>,
            _ctx: tracing_subscriber::layer::Context<'_, S>,
        ) {
            struct Visitor(std::collections::HashMap<String, String>);
            impl tracing::field::Visit for Visitor {
                fn record_debug(
                    &mut self,
                    field: &tracing::field::Field,
                    value: &dyn std::fmt::Debug,
                ) {
                    self.0
                        .insert(field.name().to_string(), format!("{value:?}"));
                }
                fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
                    self.0.insert(field.name().to_string(), value.to_string());
                }
            }
            let mut visitor = Visitor(std::collections::HashMap::new());
            event.record(&mut visitor);
            self.0.0.lock().unwrap().push(visitor.0);
        }
    }

    fn capture_events(run: impl FnOnce()) -> Vec<std::collections::HashMap<String, String>> {
        use tracing_subscriber::layer::SubscriberExt as _;

        let events = CapturedEvents::default();
        let subscriber = tracing_subscriber::registry().with(CaptureLayer(events.clone()));
        tracing::subscriber::with_default(subscriber, run);
        events.0.lock().unwrap().clone()
    }

    #[test]
    fn log_run_outcome_logs_an_info_line_with_exit_code_zero_on_success() {
        let events = capture_events(|| log_run_outcome(0));
        assert_eq!(events.len(), 1);
        let fields = &events[0];
        assert_eq!(
            fields.get("message").map(String::as_str),
            Some("job run outcome before artifact upload: success")
        );
        assert_eq!(fields.get("exit_code").map(String::as_str), Some("0"));
    }

    #[test]
    fn log_run_outcome_logs_an_error_line_with_the_nonzero_exit_code_on_failure() {
        let events = capture_events(|| log_run_outcome(1));
        assert_eq!(events.len(), 1);
        let fields = &events[0];
        assert_eq!(
            fields.get("message").map(String::as_str),
            Some("job run outcome before artifact upload: failure")
        );
        assert_eq!(fields.get("exit_code").map(String::as_str), Some("1"));
    }
}
