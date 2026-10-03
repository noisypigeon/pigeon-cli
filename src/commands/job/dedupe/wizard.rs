use std::fs;
use std::path::{Path, PathBuf};

use dialoguer::Input;

use crate::commands::FAILURE_EXIT_CODE;
use crate::commands::job::shared_wizard::{
    ConfirmInput, CpuConcurrencyInput, UploadConcurrencyInput, UploadTargetInput,
};
use crate::commands::keyring::store::Store;
use crate::core::job::Job;
use crate::core::keyring::credentials;
use crate::core::wizard::WizardInput;

use super::DedupeJob;
use super::manifest::{self, TypeSummary};
use super::worker;

/// Resolves which bucket-config to pull from -- mandatory (unlike
/// `UploadTargetInput`'s optional upload target). Own local copy --
/// `pull_transform::wizard`'s `SourceBucketInput` is a private struct,
/// unreachable from this sibling module.
struct SourceBucketInput<'a> {
    flag: Option<String>,
    store: &'a Store,
}

impl WizardInput for SourceBucketInput<'_> {
    type Value = String;

    fn flag_value(&self) -> Option<Result<String, String>> {
        self.flag.clone().map(Ok)
    }

    fn prompt(&self) -> Result<String, String> {
        self.store
            .prompt_select_bucket()
            .map(|bucket_config| bucket_config.alias.clone())
    }

    fn non_interactive_fallback(&self) -> Result<String, String> {
        Err("--source-bucket is required when not running interactively".to_string())
    }
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

fn fail(message: impl std::fmt::Display) -> i32 {
    eprintln!("Error: {message}");
    FAILURE_EXIT_CODE
}

/// Entry point for `pigeon job run dedupe` (ADR-0082).
pub fn dispatch(
    source_bucket: Option<String>,
    local_output: Option<PathBuf>,
    remote_output: Option<String>,
    concurrency: Option<usize>,
    upload_concurrency: Option<usize>,
    upload_only: bool,
    yes: bool,
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
        remote_output,
        concurrency,
        upload_concurrency,
        upload_only,
        yes,
    ))
}

async fn dispatch_async(
    source_bucket: Option<String>,
    local_output: Option<PathBuf>,
    remote_output: Option<String>,
    concurrency: Option<usize>,
    upload_concurrency: Option<usize>,
    upload_only: bool,
    yes: bool,
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
            remote_output,
            upload_concurrency,
            yes,
            &keyring_store,
        )
        .await;
    }

    let source_alias = match (SourceBucketInput {
        flag: source_bucket,
        store: &keyring_store,
    })
    .resolve()
    {
        Ok(alias) => alias,
        Err(err) => return fail(err),
    };
    let source_bucket_config = match keyring_store
        .bucket_configs()
        .find(|bucket_config| bucket_config.alias == source_alias)
    {
        Some(bucket_config) => bucket_config.clone(),
        None => return fail(format!("no bucket-config named '{source_alias}'")),
    };
    let source_secret = match credentials::get_secret(&source_bucket_config.alias) {
        Ok(secret) => secret,
        Err(err) => return fail(err),
    };

    let local_output = match (LocalOutputInput { flag: local_output }).resolve() {
        Ok(path) => path,
        Err(err) => return fail(err),
    };

    let mut job = DedupeJob {
        source_bucket: source_bucket_config,
        source_secret,
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
        flag: remote_output,
        store: &keyring_store,
    })
    .resolve()
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

    let concurrency = match (CpuConcurrencyInput { flag: concurrency }).resolve() {
        Ok(value) => value,
        Err(err) => return fail(err),
    };
    let upload_concurrency = match (UploadConcurrencyInput {
        flag: upload_concurrency,
    })
    .resolve()
    {
        Ok(value) => value,
        Err(err) => return fail(err),
    };

    match (ConfirmInput { yes }).resolve() {
        Ok(true) => {}
        Ok(false) => {
            println!("Cancelled.");
            return 0;
        }
        Err(err) => return fail(err),
    }

    let report_path = job.local_output.join("dedupe-report.txt");
    match job.run(plan, concurrency, upload_concurrency).await {
        Ok(summary) => {
            println!(
                "Processed {} file(s), {} failed ({} download, {} archive, {} hash), {} duplicate(s) skipped, {} uploaded, {} unchanged, {} upload failed.",
                summary.processed,
                summary.failed,
                summary.failure_breakdown.download,
                summary.failure_breakdown.archive,
                summary.failure_breakdown.hash,
                summary.duplicates_skipped,
                summary.uploaded,
                summary.unchanged,
                summary.upload_failed
            );
            println!("Report: {}", report_path.display());
            crate::observability::metrics::record_job_summary(
                "dedupe",
                summary.processed as u64,
                summary.failed as u64,
                summary.uploaded as u64,
                summary.unchanged as u64,
                summary.upload_failed as u64,
            );
            if summary.failed > 0 || summary.upload_failed > 0 {
                FAILURE_EXIT_CODE
            } else {
                0
            }
        }
        Err(err) => fail(err),
    }
}

/// Whether `local_output` holds a completed prior dedupe run that
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
/// an already-completed local dedupe run, skipping the bucket listing/
/// download/hash/placement phases -- and the source bucket credentials
/// they'd otherwise need -- entirely.
async fn dispatch_upload_only(
    local_output: Option<PathBuf>,
    remote_output: Option<String>,
    upload_concurrency: Option<usize>,
    yes: bool,
    keyring_store: &Store,
) -> i32 {
    let local_output = match (LocalOutputInput { flag: local_output }).resolve() {
        Ok(path) => path,
        Err(err) => return fail(err),
    };

    if !upload_only_preflight_ok(&local_output) {
        return fail(format!(
            "no completed dedupe run found under {}; run without --upload-only first",
            local_output.display()
        ));
    }

    let remote_alias = match (UploadTargetInput {
        flag: remote_output,
        store: keyring_store,
    })
    .resolve()
    {
        Ok(Some(alias)) => alias,
        Ok(None) => return fail("--remote-output is required with --upload-only"),
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
    .resolve()
    {
        Ok(value) => value,
        Err(err) => return fail(err),
    };

    match (ConfirmInput { yes }).resolve() {
        Ok(true) => {}
        Ok(false) => {
            println!("Cancelled.");
            return 0;
        }
        Err(err) => return fail(err),
    }

    match worker::run_upload_only(
        &local_output,
        (&remote_bucket_config, &remote_secret),
        upload_concurrency,
    )
    .await
    {
        Ok(summary) => {
            println!(
                "Uploaded {} file(s), {} unchanged, {} upload failed.",
                summary.uploaded, summary.unchanged, summary.upload_failed
            );
            if summary.upload_failed > 0 {
                FAILURE_EXIT_CODE
            } else {
                0
            }
        }
        Err(err) => fail(err),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn format_bytes_stays_in_bytes_under_a_kib() {
        assert_eq!(format_bytes(512), "512 B");
    }

    #[test]
    fn format_bytes_uses_larger_units_for_larger_sizes() {
        assert_eq!(format_bytes(1024), "1.0 KB");
    }
}
