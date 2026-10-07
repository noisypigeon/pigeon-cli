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
use crate::commands::keyring::bucket::client::{self, UploadBody};
use crate::commands::keyring::bucket::store::BucketConfig;
use crate::commands::keyring::store::Store;
use crate::core::keyring::credentials;
use crate::core::wizard::WizardInput;
use crate::observability::{self, transcript::Transcript};

/// Resolves `--report-bucket` (mandatory, ADR-0100) and fetches its
/// `BucketConfig`/secret -- the same three-step lookup every job's other
/// bucket-config inputs already do (e.g. `SourceBucketInput`'s call sites).
pub(crate) fn resolve(
    flag: Option<String>,
    store: &Store,
) -> Result<(BucketConfig, String), String> {
    let alias = (ReportBucketInput { flag, store }).resolve()?;
    let bucket_config = store
        .bucket_configs()
        .find(|bucket_config| bucket_config.alias == alias)
        .ok_or_else(|| format!("no bucket-config named '{alias}'"))?
        .clone();
    let secret = credentials::get_secret(&bucket_config.alias)?;
    Ok((bucket_config, secret))
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

/// Uploads `report_path`, the shared `pigeon.jsonl` log, and
/// `transcript_path` to `bucket_config` under `{prefix}/`. Best-effort: a
/// failure here is `tracing::warn!`-logged but never changes the calling
/// job's own exit code (ADR-0100) -- this is instrumentation about the run,
/// not the run's primary deliverable.
pub(crate) async fn upload_run_artifacts(
    bucket_config: &BucketConfig,
    secret: &str,
    prefix: &str,
    report_path: &Path,
    transcript_path: &Path,
) {
    let uploads: [(&str, &Path); 2] = [
        ("report.txt", report_path),
        ("transcript.txt", transcript_path),
    ];
    for (name, path) in uploads {
        upload_one(bucket_config, secret, &format!("{prefix}/{name}"), path).await;
    }
    match observability::log_file_path() {
        Ok(log_path) => {
            upload_one(
                bucket_config,
                secret,
                &format!("{prefix}/pigeon.jsonl"),
                &log_path,
            )
            .await;
        }
        Err(err) => {
            tracing::warn!(error = %err, "failed to resolve log file path for run artifact upload");
        }
    }
}

async fn upload_one(bucket_config: &BucketConfig, secret: &str, key: &str, path: &Path) {
    if let Err(err) = client::upload_if_changed(
        bucket_config,
        secret,
        key,
        UploadBody::Path(path.to_path_buf()),
    )
    .await
    {
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
            write_summary_report(dir.path(), "pull-transform", &Summary { processed: 3 }).unwrap();
        let contents = fs::read_to_string(&path).unwrap();
        assert!(contents.contains("processed: 3"));
        assert_eq!(path.file_name().unwrap(), "pull-transform-report.txt");
    }

    #[test]
    fn write_summary_report_also_works_for_an_error_string() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_summary_report(dir.path(), "email-sync", &"boom".to_string()).unwrap();
        let contents = fs::read_to_string(&path).unwrap();
        assert!(contents.contains("boom"));
    }
}
