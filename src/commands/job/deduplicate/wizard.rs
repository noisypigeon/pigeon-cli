use std::fs;
use std::path::{Path, PathBuf};

use dialoguer::{Input, MultiSelect, theme::ColorfulTheme};

use crate::commands::job::report_upload;
use crate::commands::job::shared_wizard::{
    ConfirmInput, CpuConcurrencyInput, UploadConcurrencyInput, UploadTargetInput,
};
use crate::commands::keyring::bucket::store::BucketConfig;
use crate::commands::keyring::store::Store;
use crate::commands::{FAILURE_EXIT_CODE, fail};
use crate::core::job::Job;
use crate::core::keyring::credentials;
use crate::core::wizard::WizardInput;

use super::DeduplicateJob;
use super::manifest::{self, TypeSummary};
use super::worker;

/// Resolves which bucket-config(s) to pull from and combine into one
/// shared staging tree -- mandatory, at least one required (ADR-0109).
/// Deliberately a `deduplicate`-local type, not a generalization of
/// `shared_wizard::SourceBucketInput` (which stays single-alias for its
/// other consumer, `pull_transform::wizard`) -- per this codebase's
/// "duplicate until the third consumer" precedent (ADR-0082's own
/// Out-of-scope section), one consumer doesn't justify generalizing a
/// shared type. Shaped after `email_sync::wizard::IdentitiesInput`, the
/// closest existing "mandatory, no-safe-default, multi-value, alias-
/// resolved" wizard input.
struct SourceBucketsInput<'a> {
    flag: Vec<String>,
    store: &'a Store,
}

impl WizardInput for SourceBucketsInput<'_> {
    type Value = Vec<String>;

    fn flag_value(&self) -> Option<Result<Vec<String>, String>> {
        if self.flag.is_empty() {
            return None;
        }
        Some(
            self.flag
                .iter()
                .map(|alias| {
                    self.store
                        .bucket_configs()
                        .find(|bucket_config| &bucket_config.alias == alias)
                        .map(|_| alias.clone())
                        .ok_or_else(|| format!("no bucket-config named '{alias}'"))
                })
                .collect(),
        )
    }

    fn prompt(&self) -> Result<Vec<String>, String> {
        let all: Vec<&BucketConfig> = self.store.bucket_configs().collect();
        if all.is_empty() {
            return Err(
                "no bucket-configs configured; run 'pigeon keyring add bucket' first".to_string(),
            );
        }
        let labels: Vec<String> = all
            .iter()
            .map(|bucket_config| {
                format!(
                    "{} ({}, {})",
                    bucket_config.alias, bucket_config.endpoint, bucket_config.bucket
                )
            })
            .collect();
        let selected = MultiSelect::with_theme(&ColorfulTheme::default())
            .with_prompt("Select source bucket-config(s)")
            .items(&labels)
            .interact()
            .map_err(|err| format!("failed to read bucket selection: {err}"))?;
        if selected.is_empty() {
            return Err("at least one source bucket-config must be selected".to_string());
        }
        Ok(selected
            .into_iter()
            .map(|index| all[index].alias.clone())
            .collect())
    }

    fn non_interactive_fallback(&self) -> Result<Vec<String>, String> {
        Err("--source-bucket is required when not running interactively".to_string())
    }
}

/// Looks up `alias` against `store`'s configured bucket-configs and fetches
/// its keychain secret -- the same three-step lookup every job's other
/// bucket-config inputs already do (e.g. `report_upload::resolve`). Used to
/// resolve each of `SourceBucketsInput`'s already-validated aliases into a
/// usable `(BucketConfig, secret)` pair.
fn resolve_bucket(alias: &str, store: &Store) -> Result<(BucketConfig, String), String> {
    let bucket_config = store
        .bucket_configs()
        .find(|bucket_config| bucket_config.alias == alias)
        .ok_or_else(|| format!("no bucket-config named '{alias}'"))?
        .clone();
    let secret = credentials::get_secret(&bucket_config.alias)?;
    Ok((bucket_config, secret))
}

fn default_local_output() -> PathBuf {
    std::env::temp_dir().join("pigeon-job")
}

struct LocalOutputInput {
    flag: Option<PathBuf>,
}

impl WizardInput for LocalOutputInput {
    type Value = PathBuf;

    fn flag_value(&self) -> Option<Result<PathBuf, String>> {
        self.flag.clone().map(Ok)
    }

    fn prompt(&self) -> Result<PathBuf, String> {
        let default = default_local_output();
        let value = Input::<String>::new()
            .with_prompt("Local directory to stage and store output under")
            .default(default.display().to_string())
            .interact_text()
            .map_err(|err| format!("failed to read local output directory: {err}"))?;
        Ok(PathBuf::from(value))
    }

    fn non_interactive_fallback(&self) -> Result<PathBuf, String> {
        Ok(default_local_output())
    }
}

/// Creates `--local-output` if it doesn't exist yet (ADR-0111) -- without
/// this, a fresh path fails later with a raw OS error from
/// `report_upload::new_transcript`, the first step that actually requires
/// the directory to exist. Matches the pattern `rclone copy`/`rclone
/// delete` already use (ADR-0110). Deliberately not called from
/// `dispatch_upload_only`: that path requires `--local-output` to already
/// hold a completed prior run, so auto-creating a missing directory there
/// would mask "nothing to resume" with a different, equally wrong, empty
/// state.
fn ensure_local_output_dir(path: &Path) -> Result<(), String> {
    fs::create_dir_all(path).map_err(|err| format!("failed to create {}: {err}", path.display()))
}

fn format_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

/// Prints the pre-run per-extension type summary table -- reflects only
/// pending (not yet checkpointed) objects, and doesn't yet know what's
/// inside any zip (unexpanded, shown as its own `zip` row).
fn print_type_summary(summaries: &[TypeSummary]) {
    let rows: Vec<Vec<String>> = summaries
        .iter()
        .map(|summary| {
            vec![
                summary.extension.clone(),
                summary.count.to_string(),
                format_bytes(summary.total_bytes),
            ]
        })
        .collect();
    crate::commands::print_table(&["EXTENSION", "PENDING", "SIZE"], &rows);
}

/// Entry point for `pigeon job run deduplicate` (ADR-0082).
#[allow(clippy::too_many_arguments)]
pub fn dispatch(
    source_bucket: Vec<String>,
    local_output: Option<PathBuf>,
    destination_bucket: Option<String>,
    concurrency: Option<usize>,
    upload_concurrency: Option<usize>,
    upload_only: bool,
    report_bucket: Option<String>,
    job_name: &'static str,
    non_interactive: bool,
) -> i32 {
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(err) => return fail(format!("failed to start async runtime: {err}")),
    };
    runtime.block_on(dispatch_async(
        source_bucket,
        local_output,
        destination_bucket,
        concurrency,
        upload_concurrency,
        upload_only,
        report_bucket,
        job_name,
        non_interactive,
    ))
}

#[allow(clippy::too_many_arguments)]
async fn dispatch_async(
    source_bucket: Vec<String>,
    local_output: Option<PathBuf>,
    destination_bucket: Option<String>,
    concurrency: Option<usize>,
    upload_concurrency: Option<usize>,
    upload_only: bool,
    report_bucket: Option<String>,
    job_name: &'static str,
    non_interactive: bool,
) -> i32 {
    // Held for this whole async fn's lifetime, same discipline as every
    // other job's dispatch_async (ADR-0073).
    let _sampler =
        crate::observability::resources::ResourceSampler::spawn(std::time::Duration::from_secs(5));

    let keyring_store_path = match Store::default_path() {
        Ok(path) => path,
        Err(err) => return fail(err),
    };
    let keyring_store = match Store::load(&keyring_store_path) {
        Ok(store) => store,
        Err(err) => return fail(err),
    };

    if upload_only {
        return dispatch_upload_only(
            local_output,
            destination_bucket,
            upload_concurrency,
            report_bucket,
            job_name,
            non_interactive,
            &keyring_store,
        )
        .await;
    }

    let source_aliases = match (SourceBucketsInput {
        flag: source_bucket,
        store: &keyring_store,
    })
    .resolve(non_interactive)
    {
        Ok(aliases) => aliases,
        Err(err) => return fail(err),
    };
    let source_buckets = match source_aliases
        .iter()
        .map(|alias| resolve_bucket(alias, &keyring_store))
        .collect::<Result<Vec<_>, _>>()
    {
        Ok(buckets) => buckets,
        Err(err) => return fail(err),
    };

    let local_output = match (LocalOutputInput { flag: local_output }).resolve(non_interactive) {
        Ok(path) => path,
        Err(err) => return fail(err),
    };

    let mut job = DeduplicateJob {
        source_buckets,
        local_output,
        remote: None,
    };
    let plan = match job.gather().await {
        Ok(plan) => plan,
        Err(err) => return fail(err),
    };

    print_type_summary(&plan.type_summary);
    if plan.tasks.is_empty() {
        println!("Everything is already up to date.");
        return 0;
    }
    println!("{} pending object(s) found.", plan.tasks.len());

    let resolved_remote_alias = match (UploadTargetInput {
        flag: destination_bucket,
        store: &keyring_store,
    })
    .resolve(non_interactive)
    {
        Ok(alias) => alias,
        Err(err) => return fail(err),
    };
    job.remote = match resolved_remote_alias {
        Some(alias) => {
            let bucket_config = match keyring_store.bucket_configs().find(|b| b.alias == alias) {
                Some(bucket_config) => bucket_config.clone(),
                None => return fail(format!("no bucket-config named '{alias}'")),
            };
            let secret = match credentials::get_secret(&bucket_config.alias) {
                Ok(secret) => secret,
                Err(err) => return fail(err),
            };
            Some((bucket_config, secret))
        }
        None => None,
    };

    let concurrency = match (CpuConcurrencyInput { flag: concurrency }).resolve(non_interactive) {
        Ok(value) => value,
        Err(err) => return fail(err),
    };
    let upload_concurrency = match (UploadConcurrencyInput {
        flag: upload_concurrency,
    })
    .resolve(non_interactive)
    {
        Ok(value) => value,
        Err(err) => return fail(err),
    };
    let (report_bucket_config, report_secret) =
        match report_upload::resolve(report_bucket, &keyring_store, non_interactive) {
            Ok(value) => value,
            Err(err) => return fail(err),
        };

    match (ConfirmInput { non_interactive }).resolve(non_interactive) {
        Ok(true) => {}
        Ok(false) => {
            println!("Cancelled.");
            return 0;
        }
        Err(err) => return fail(err),
    }

    if let Err(err) = ensure_local_output_dir(&job.local_output) {
        return fail(err);
    }

    let run_id = report_upload::generate_run_id();
    let run_prefix = report_upload::run_prefix(job_name, &run_id);
    let (transcript, transcript_path) = match report_upload::new_transcript(&job.local_output) {
        Ok(value) => value,
        Err(err) => return fail(err),
    };

    let job_local_output = job.local_output.clone();
    let report_path = job_local_output.join("deduplicate-report.txt");
    let exit_code = match job.run(plan, concurrency, upload_concurrency).await {
        Ok(summary) => {
            let mut message = format!(
                "Processed {} file(s), {} failed ({} download, {} archive, {} hash), {} duplicate(s) skipped, {} zip member(s) dropped (extraction cap), {} AppleDouble object(s) skipped, {} uploaded, {} unchanged, {} upload failed.",
                summary.processed,
                summary.failed,
                summary.failure_breakdown.download,
                summary.failure_breakdown.archive,
                summary.failure_breakdown.hash,
                summary.duplicates_skipped,
                summary.dropped_members,
                summary.skipped_apple_double,
                summary.uploaded,
                summary.unchanged,
                summary.upload_failed
            );
            if !summary.archive_failures.is_empty() {
                message.push_str(&format!(
                    " {} archive(s) need manual attention (see report).",
                    summary.archive_failures.len()
                ));
            }
            report_upload::say(&transcript, message);
            report_upload::say(&transcript, format!("Report: {}", report_path.display()));
            if summary.failed > 0 || summary.upload_failed > 0 {
                FAILURE_EXIT_CODE
            } else {
                0
            }
        }
        Err(err) => {
            report_upload::say_error(&transcript, &err);
            if !report_path.exists() {
                let _ = report_upload::write_summary_report(&job_local_output, job_name, &err);
            }
            fail(err)
        }
    };
    report_upload::log_run_outcome(exit_code);
    report_upload::upload_run_artifacts(
        &report_bucket_config,
        &report_secret,
        &run_prefix,
        &report_path,
        &transcript_path,
    )
    .await;
    exit_code
}

/// Whether `local_output` holds a completed prior deduplicate run that
/// `--upload-only` can resume uploading from: its `.staging/.processed`
/// checkpoint must exist (there was a run at all) and its `result/` must be
/// non-empty (there's something to upload) (ADR-0089).
fn upload_only_preflight_ok(local_output: &Path) -> bool {
    let processed_marker = local_output
        .join(".staging")
        .join(manifest::PROCESSED_FILE_NAME);
    if !processed_marker.exists() {
        return false;
    }
    fs::read_dir(local_output.join("result"))
        .map(|mut entries| entries.next().is_some())
        .unwrap_or(false)
}

/// `--upload-only` branch of `dispatch_async` (ADR-0089): resumes uploading
/// an already-completed local deduplicate run, skipping the bucket listing/
/// download/hash/placement phases -- and the source bucket credentials
/// they'd otherwise need -- entirely.
#[allow(clippy::too_many_arguments)]
async fn dispatch_upload_only(
    local_output: Option<PathBuf>,
    destination_bucket: Option<String>,
    upload_concurrency: Option<usize>,
    report_bucket: Option<String>,
    job_name: &'static str,
    non_interactive: bool,
    keyring_store: &Store,
) -> i32 {
    let local_output = match (LocalOutputInput { flag: local_output }).resolve(non_interactive) {
        Ok(path) => path,
        Err(err) => return fail(err),
    };

    if !upload_only_preflight_ok(&local_output) {
        return fail(format!(
            "no completed deduplicate run found under {}; run without --upload-only first",
            local_output.display()
        ));
    }

    let remote_alias = match (UploadTargetInput {
        flag: destination_bucket,
        store: keyring_store,
    })
    .resolve(non_interactive)
    {
        Ok(Some(alias)) => alias,
        Ok(None) => return fail("--destination-bucket is required with --upload-only"),
        Err(err) => return fail(err),
    };
    let remote_bucket_config = match keyring_store
        .bucket_configs()
        .find(|bucket_config| bucket_config.alias == remote_alias)
    {
        Some(bucket_config) => bucket_config.clone(),
        None => return fail(format!("no bucket-config named '{remote_alias}'")),
    };
    let remote_secret = match credentials::get_secret(&remote_bucket_config.alias) {
        Ok(secret) => secret,
        Err(err) => return fail(err),
    };

    let upload_concurrency = match (UploadConcurrencyInput {
        flag: upload_concurrency,
    })
    .resolve(non_interactive)
    {
        Ok(value) => value,
        Err(err) => return fail(err),
    };
    let (report_bucket_config, report_secret) =
        match report_upload::resolve(report_bucket, keyring_store, non_interactive) {
            Ok(value) => value,
            Err(err) => return fail(err),
        };

    match (ConfirmInput { non_interactive }).resolve(non_interactive) {
        Ok(true) => {}
        Ok(false) => {
            println!("Cancelled.");
            return 0;
        }
        Err(err) => return fail(err),
    }

    let run_id = report_upload::generate_run_id();
    let run_prefix = report_upload::run_prefix(job_name, &run_id);
    let (transcript, transcript_path) = match report_upload::new_transcript(&local_output) {
        Ok(value) => value,
        Err(err) => return fail(err),
    };

    let (exit_code, report_path) = match worker::run_upload_only(
        &local_output,
        (&remote_bucket_config, &remote_secret),
        upload_concurrency,
    )
    .await
    {
        Ok(summary) => {
            let message = format!(
                "Uploaded {} file(s), {} unchanged, {} upload failed.",
                summary.uploaded, summary.unchanged, summary.upload_failed
            );
            report_upload::say(&transcript, message);
            let exit_code = if summary.upload_failed > 0 {
                FAILURE_EXIT_CODE
            } else {
                0
            };
            let report_path =
                report_upload::write_summary_report(&local_output, job_name, &summary)
                    .unwrap_or_else(|err| {
                        tracing::warn!(error = %err, "failed to write report");
                        local_output.join(format!("{job_name}-report.txt"))
                    });
            (exit_code, report_path)
        }
        Err(err) => {
            report_upload::say_error(&transcript, &err);
            let report_path = report_upload::write_summary_report(&local_output, job_name, &err)
                .unwrap_or_else(|_| local_output.join(format!("{job_name}-report.txt")));
            (fail(err), report_path)
        }
    };
    report_upload::log_run_outcome(exit_code);
    report_upload::upload_run_artifacts(
        &report_bucket_config,
        &report_secret,
        &run_prefix,
        &report_path,
        &transcript_path,
    )
    .await;
    exit_code
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transcript_contains_the_error_message_after_a_post_creation_failure() {
        let dir = tempfile::tempdir().unwrap();
        let (transcript, transcript_path) = report_upload::new_transcript(dir.path()).unwrap();
        report_upload::say_error(&transcript, "simulated deduplicate failure");
        let contents = fs::read_to_string(&transcript_path).unwrap();
        assert!(!contents.is_empty());
        assert!(contents.contains("simulated deduplicate failure"));
    }

    #[test]
    fn upload_only_preflight_fails_without_a_processed_checkpoint() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("result")).unwrap();
        fs::write(dir.path().join("result").join("a.txt"), b"a").unwrap();

        assert!(!upload_only_preflight_ok(dir.path()));
    }

    #[test]
    fn upload_only_preflight_fails_with_an_empty_result_dir() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join(".staging")).unwrap();
        fs::write(
            dir.path()
                .join(".staging")
                .join(manifest::PROCESSED_FILE_NAME),
            b"a.txt\n",
        )
        .unwrap();
        fs::create_dir_all(dir.path().join("result")).unwrap();

        assert!(!upload_only_preflight_ok(dir.path()));
    }

    #[test]
    fn upload_only_preflight_passes_for_a_completed_run() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join(".staging")).unwrap();
        fs::write(
            dir.path()
                .join(".staging")
                .join(manifest::PROCESSED_FILE_NAME),
            b"a.txt\n",
        )
        .unwrap();
        fs::create_dir_all(dir.path().join("result")).unwrap();
        fs::write(dir.path().join("result").join("a.txt"), b"a").unwrap();

        assert!(upload_only_preflight_ok(dir.path()));
    }

    #[test]
    fn default_local_output_is_under_the_os_temp_dir() {
        let path = default_local_output();
        assert!(path.starts_with(std::env::temp_dir()));
        assert_eq!(path.file_name().unwrap(), "pigeon-job");
    }

    #[test]
    fn ensure_local_output_dir_creates_a_missing_nested_path() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("does").join("not").join("exist-yet");
        assert!(!target.exists());

        ensure_local_output_dir(&target).unwrap();

        assert!(target.is_dir());
    }

    #[test]
    fn ensure_local_output_dir_is_a_no_op_when_the_directory_already_exists() {
        let dir = tempfile::tempdir().unwrap();

        ensure_local_output_dir(dir.path()).unwrap();

        assert!(dir.path().is_dir());
    }

    #[test]
    fn ensure_local_output_dir_reports_a_clear_error_on_failure() {
        let dir = tempfile::tempdir().unwrap();
        // A plain file where a path component needs to be a directory, so
        // `create_dir_all` fails with a real OS error (ENOTDIR) rather than
        // succeeding -- confirms the error message names the offending path.
        let blocker = dir.path().join("blocker");
        fs::write(&blocker, b"not a directory").unwrap();
        let target = blocker.join("nested");

        let err = ensure_local_output_dir(&target).unwrap_err();

        assert!(err.contains("failed to create"));
        assert!(err.contains(&target.display().to_string()));
    }

    #[test]
    fn format_bytes_stays_in_bytes_under_a_kib() {
        assert_eq!(format_bytes(512), "512 B");
    }

    #[test]
    fn format_bytes_uses_larger_units_for_larger_sizes() {
        assert_eq!(format_bytes(1024), "1.0 KB");
    }
}
