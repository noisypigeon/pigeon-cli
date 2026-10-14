use std::path::PathBuf;

use dialoguer::Input;

use crate::commands::job::report_upload;
use crate::commands::job::shared_wizard::{
    CheckersInput, ConfirmInput, TpslimitInput, TransfersInput, load_keyring, prepare_rclone,
};
use crate::commands::keyring::store::Store;
use crate::commands::{FAILURE_EXIT_CODE, fail};
use crate::core::job::Job;
use crate::core::rclone::config::RemoteTarget;
use crate::core::wizard::WizardInput;

use super::{RcloneCopyJob, RcloneDeleteJob};

/// Resolves `--source-bucket`, a pigeon keyring `BucketConfig` alias
/// (ADR-0136, reversing ADR-0101's original raw-passthrough design) --
/// `rclone` only ever sees a config file pigeon itself generates from this
/// alias, never an externally-provisioned or user-general-purpose
/// `rclone.conf`. Structurally identical to
/// `shared_wizard::SourceBucketInput`'s flag/prompt/error shape; kept local
/// since no second consumer exists yet. Shared by both `copy` and `delete`.
///
/// Accepts the composed `alias[:prefix/]` form (ADR-0138) -- a bare alias
/// is the whole bucket root, as it was under ADR-0136. Unlike
/// `transform`'s version there is no prefix default: this job has no
/// file-type concept to derive one from, so an omitted prefix stays the
/// root. The interactive picker yields a bucket root too; a prefix is
/// expressible via the flag.
struct SourceBucketInput<'a> {
    flag: Option<String>,
    store: &'a Store,
}

impl WizardInput for SourceBucketInput<'_> {
    type Value = RemoteTarget;

    fn flag_value(&self) -> Option<Result<RemoteTarget, String>> {
        self.flag
            .as_deref()
            .map(|value| RemoteTarget::parse(value, self.store))
    }

    fn prompt(&self) -> Result<RemoteTarget, String> {
        match self.store.prompt_select_bucket() {
            Ok(bucket_config) => Ok(RemoteTarget::bucket_root(&bucket_config.alias)),
            Err(message) => Err(message),
        }
    }

    fn non_interactive_fallback(&self) -> Result<RemoteTarget, String> {
        Err("--source-bucket is required when not running interactively".to_string())
    }
}

/// Resolves `--destination-bucket`. Same shape as `SourceBucketInput`,
/// composed form included. `copy` only -- `delete` has no destination.
struct DestinationBucketInput<'a> {
    flag: Option<String>,
    store: &'a Store,
}

impl WizardInput for DestinationBucketInput<'_> {
    type Value = RemoteTarget;

    fn flag_value(&self) -> Option<Result<RemoteTarget, String>> {
        self.flag
            .as_deref()
            .map(|value| RemoteTarget::parse(value, self.store))
    }

    fn prompt(&self) -> Result<RemoteTarget, String> {
        match self.store.prompt_select_bucket() {
            Ok(bucket_config) => Ok(RemoteTarget::bucket_root(&bucket_config.alias)),
            Err(message) => Err(message),
        }
    }

    fn non_interactive_fallback(&self) -> Result<RemoteTarget, String> {
        Err("--destination-bucket is required when not running interactively".to_string())
    }
}

fn default_local_output() -> PathBuf {
    std::env::temp_dir().join("pigeon-job")
}

/// Where this run's rclone log (also serving as this job's report) and
/// transcript are written -- unlike every other job, not a staging area
/// for transferred data, since rclone transfers directly source ->
/// destination with no pigeon-side staging. Shared by both actions.
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
            .with_prompt("Local directory this run's rclone log and transcript are written under")
            .default(default.display().to_string())
            .interact_text()
            .map_err(|err| format!("failed to read local output directory: {err}"))?;
        Ok(PathBuf::from(value))
    }

    fn non_interactive_fallback(&self) -> Result<PathBuf, String> {
        Ok(default_local_output())
    }
}

/// Entry point for `pigeon job run rclone copy` (ADR-0101, restructured by
/// ADR-0110).
#[allow(clippy::too_many_arguments)]
pub fn dispatch_copy(
    source_bucket: Option<String>,
    destination_bucket: Option<String>,
    local_output: Option<PathBuf>,
    report_bucket: Option<String>,
    transfers: Option<usize>,
    checkers: Option<usize>,
    tpslimit: Option<usize>,
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
    runtime.block_on(dispatch_copy_async(
        source_bucket,
        destination_bucket,
        local_output,
        report_bucket,
        transfers,
        checkers,
        tpslimit,
        job_name,
        non_interactive,
    ))
}

#[allow(clippy::too_many_arguments)]
async fn dispatch_copy_async(
    source_bucket: Option<String>,
    destination_bucket: Option<String>,
    local_output: Option<PathBuf>,
    report_bucket: Option<String>,
    transfers: Option<usize>,
    checkers: Option<usize>,
    tpslimit: Option<usize>,
    job_name: &'static str,
    non_interactive: bool,
) -> i32 {
    // Held for this whole async fn's lifetime -- every early `return
    // fail(...)` below drops it, aborting the sampling task automatically
    // (ADR-0073).
    let _sampler =
        crate::observability::resources::ResourceSampler::spawn(std::time::Duration::from_secs(5));

    let keyring_store = match load_keyring() {
        Ok(store) => store,
        Err(err) => return fail(err),
    };
    if let Err(err) = prepare_rclone(&keyring_store).await {
        return fail(err);
    }

    let source = match (SourceBucketInput {
        flag: source_bucket,
        store: &keyring_store,
    })
    .resolve(non_interactive)
    {
        Ok(target) => target.address(),
        Err(err) => return fail(err),
    };
    let destination = match (DestinationBucketInput {
        flag: destination_bucket,
        store: &keyring_store,
    })
    .resolve(non_interactive)
    {
        Ok(target) => target.address(),
        Err(err) => return fail(err),
    };
    let local_output = match (LocalOutputInput { flag: local_output }).resolve(non_interactive) {
        Ok(value) => value,
        Err(err) => return fail(err),
    };
    let transfers = match (TransfersInput { flag: transfers }).resolve(non_interactive) {
        Ok(value) => value,
        Err(err) => return fail(err),
    };
    let checkers = match (CheckersInput { flag: checkers }).resolve(non_interactive) {
        Ok(value) => value,
        Err(err) => return fail(err),
    };
    let tpslimit = match (TpslimitInput { flag: tpslimit }).resolve(non_interactive) {
        Ok(value) => value,
        Err(err) => return fail(err),
    };
    let report_bucket_config =
        match report_upload::resolve(report_bucket, &keyring_store, non_interactive) {
            Ok(value) => value,
            Err(err) => return fail(err),
        };

    println!("Source:      {source}");
    println!("Destination: {destination}");

    match (ConfirmInput { non_interactive }).resolve(non_interactive) {
        Ok(true) => {}
        Ok(false) => {
            println!("Cancelled.");
            return 0;
        }
        Err(err) => return fail(err),
    }

    if let Err(err) = std::fs::create_dir_all(&local_output) {
        return fail(format!(
            "failed to create {}: {err}",
            local_output.display()
        ));
    }

    let run_id = report_upload::generate_run_id();
    let run_prefix = report_upload::run_prefix(job_name, &run_id);
    let (transcript, transcript_path) = match report_upload::new_transcript(&local_output) {
        Ok(value) => value,
        Err(err) => return fail(err),
    };
    let log_path = local_output.join(format!("rclone-{run_id}.jsonl"));
    let log_path_for_err = log_path.clone();

    let job = RcloneCopyJob {
        source,
        destination,
        log_path,
        transfers,
        checkers,
        tpslimit,
    };
    let plan = match job.gather().await {
        Ok(plan) => plan,
        Err(err) => return fail(err),
    };

    let (exit_code, report_path) = match job.run(plan, 1, 1).await {
        Ok(summary) => {
            let message = format!(
                "Transferred {} file(s) ({} bytes), {} error(s).",
                summary.transferred, summary.bytes, summary.errors
            );
            report_upload::say(&transcript, message);
            let exit_code = if summary.errors > 0 {
                FAILURE_EXIT_CODE
            } else {
                0
            };
            (exit_code, summary.log_path)
        }
        Err(err) => {
            report_upload::say_error(&transcript, &err);
            (fail(err), log_path_for_err)
        }
    };

    println!("Report: {}", report_path.display());

    report_upload::log_run_outcome(exit_code);
    report_upload::upload_run_artifacts(
        &report_bucket_config,
        &run_prefix,
        &run_id,
        &report_path,
        &transcript_path,
    )
    .await;

    exit_code
}

/// Entry point for `pigeon job run rclone delete` (ADR-0110).
pub fn dispatch_delete(
    source_bucket: Option<String>,
    local_output: Option<PathBuf>,
    report_bucket: Option<String>,
    checkers: Option<usize>,
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
    runtime.block_on(dispatch_delete_async(
        source_bucket,
        local_output,
        report_bucket,
        checkers,
        job_name,
        non_interactive,
    ))
}

async fn dispatch_delete_async(
    source_bucket: Option<String>,
    local_output: Option<PathBuf>,
    report_bucket: Option<String>,
    checkers: Option<usize>,
    job_name: &'static str,
    non_interactive: bool,
) -> i32 {
    let _sampler =
        crate::observability::resources::ResourceSampler::spawn(std::time::Duration::from_secs(5));

    let keyring_store = match load_keyring() {
        Ok(store) => store,
        Err(err) => return fail(err),
    };
    if let Err(err) = prepare_rclone(&keyring_store).await {
        return fail(err);
    }

    let source = match (SourceBucketInput {
        flag: source_bucket,
        store: &keyring_store,
    })
    .resolve(non_interactive)
    {
        Ok(target) => target.address(),
        Err(err) => return fail(err),
    };
    let local_output = match (LocalOutputInput { flag: local_output }).resolve(non_interactive) {
        Ok(value) => value,
        Err(err) => return fail(err),
    };
    let checkers = match (CheckersInput { flag: checkers }).resolve(non_interactive) {
        Ok(value) => value,
        Err(err) => return fail(err),
    };
    let report_bucket_config =
        match report_upload::resolve(report_bucket, &keyring_store, non_interactive) {
            Ok(value) => value,
            Err(err) => return fail(err),
        };

    println!("Source: {source}");
    println!("WARNING: this will recursively and permanently delete everything under this path.");

    match (ConfirmInput { non_interactive }).resolve(non_interactive) {
        Ok(true) => {}
        Ok(false) => {
            println!("Cancelled.");
            return 0;
        }
        Err(err) => return fail(err),
    }

    if let Err(err) = std::fs::create_dir_all(&local_output) {
        return fail(format!(
            "failed to create {}: {err}",
            local_output.display()
        ));
    }

    let run_id = report_upload::generate_run_id();
    let run_prefix = report_upload::run_prefix(job_name, &run_id);
    let (transcript, transcript_path) = match report_upload::new_transcript(&local_output) {
        Ok(value) => value,
        Err(err) => return fail(err),
    };
    // Distinct name from `copy`'s `rclone-{run_id}.jsonl` so a shared
    // `--local-output` directory never conflates the two actions' logs.
    let log_path = local_output.join(format!("rclone-purge-{run_id}.jsonl"));
    let log_path_for_err = log_path.clone();

    let job = RcloneDeleteJob {
        source,
        log_path,
        checkers,
    };
    let plan = match job.gather().await {
        Ok(plan) => plan,
        Err(err) => return fail(err),
    };

    let (exit_code, report_path) = match job.run(plan, 1, 1).await {
        Ok(summary) => {
            let message = format!(
                "Deleted {} object(s), {} error(s).",
                summary.deleted, summary.errors
            );
            report_upload::say(&transcript, message);
            let exit_code = if summary.errors > 0 {
                FAILURE_EXIT_CODE
            } else {
                0
            };
            (exit_code, summary.log_path)
        }
        Err(err) => {
            report_upload::say_error(&transcript, &err);
            (fail(err), log_path_for_err)
        }
    };

    println!("Report: {}", report_path.display());

    report_upload::log_run_outcome(exit_code);
    report_upload::upload_run_artifacts(
        &report_bucket_config,
        &run_prefix,
        &run_id,
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
        report_upload::say_error(&transcript, "simulated rclone copy failure");
        let contents = std::fs::read_to_string(&transcript_path).unwrap();
        assert!(!contents.is_empty());
        assert!(contents.contains("simulated rclone copy failure"));
    }

    #[test]
    fn source_bucket_input_non_interactive_fallback_errors() {
        let store = Store::default();
        let input = SourceBucketInput {
            flag: None,
            store: &store,
        };
        assert_eq!(
            input.non_interactive_fallback(),
            Err("--source-bucket is required when not running interactively".to_string())
        );
    }

    #[test]
    fn source_bucket_input_flag_value_rejects_an_unknown_alias() {
        let store = Store::default();
        let input = SourceBucketInput {
            flag: Some("nope".to_string()),
            store: &store,
        };
        assert_eq!(
            input.flag_value(),
            Some(Err("no bucket-config named 'nope'".to_string()))
        );
    }

    #[test]
    fn destination_bucket_input_non_interactive_fallback_errors() {
        let store = Store::default();
        let input = DestinationBucketInput {
            flag: None,
            store: &store,
        };
        assert_eq!(
            input.non_interactive_fallback(),
            Err("--destination-bucket is required when not running interactively".to_string())
        );
    }
}
