use std::path::PathBuf;

use dialoguer::Input;

use crate::commands::job::report_upload;
use crate::commands::job::shared_wizard::ConfirmInput;
use crate::commands::keyring::store::Store;
use crate::commands::{FAILURE_EXIT_CODE, fail};
use crate::core::job::Job;
use crate::core::wizard::WizardInput;

use super::ImportJob;
use super::worker;

/// Resolves the rclone source, e.g. `source:media/`. A plain string, not a
/// pigeon bucket-config alias -- `import` never looks this up against the
/// keyring (ADR-0101). Structurally identical to
/// `shared_wizard::SourceBucketInput`'s flag/prompt/error shape; kept local
/// since no second consumer exists yet.
struct SourceInput {
    flag: Option<String>,
}

impl WizardInput for SourceInput {
    type Value = String;

    fn flag_value(&self) -> Option<Result<String, String>> {
        self.flag.clone().map(Ok)
    }

    fn prompt(&self) -> Result<String, String> {
        Input::<String>::new()
            .with_prompt("rclone source (e.g. 'source:media/')")
            .interact_text()
            .map_err(|err| format!("failed to read source: {err}"))
    }

    fn non_interactive_fallback(&self) -> Result<String, String> {
        Err("--source is required when not running interactively".to_string())
    }
}

/// Resolves the rclone destination, e.g. `destination:`. Same shape as
/// `SourceInput`.
struct DestinationInput {
    flag: Option<String>,
}

impl WizardInput for DestinationInput {
    type Value = String;

    fn flag_value(&self) -> Option<Result<String, String>> {
        self.flag.clone().map(Ok)
    }

    fn prompt(&self) -> Result<String, String> {
        Input::<String>::new()
            .with_prompt("rclone destination (e.g. 'destination:')")
            .interact_text()
            .map_err(|err| format!("failed to read destination: {err}"))
    }

    fn non_interactive_fallback(&self) -> Result<String, String> {
        Err("--destination is required when not running interactively".to_string())
    }
}

/// Flat default for `TransfersInput` (ADR-0108) -- matches ADR-0106's
/// tuned value, now overridable rather than fixed.
const TRANSFERS_DEFAULT: usize = 8;

/// Resolves rclone's `--transfers` (concurrent file transfers). Falls
/// back to the flat default rather than erroring non-interactively
/// (ADR-0108, same `UploadConcurrencyInput` precedent): this is a new
/// flag being added to a command that already runs unattended in
/// scripts/cron today.
struct TransfersInput {
    flag: Option<usize>,
}

impl WizardInput for TransfersInput {
    type Value = usize;

    fn flag_value(&self) -> Option<Result<usize, String>> {
        self.flag.map(|value| Ok(value.max(1)))
    }

    fn prompt(&self) -> Result<usize, String> {
        let value = Input::<usize>::new()
            .with_prompt("rclone --transfers")
            .default(TRANSFERS_DEFAULT)
            .interact_text()
            .map_err(|err| format!("failed to read transfers: {err}"))?;
        Ok(value.max(1))
    }

    fn non_interactive_fallback(&self) -> Result<usize, String> {
        Ok(TRANSFERS_DEFAULT)
    }
}

/// Flat default for `CheckersInput` (ADR-0108) -- matches ADR-0106's
/// tuned value, now overridable rather than fixed.
const CHECKERS_DEFAULT: usize = 16;

/// Resolves rclone's `--checkers` (concurrent list/compare operations).
/// Same shape and non-interactive-fallback reasoning as `TransfersInput`.
struct CheckersInput {
    flag: Option<usize>,
}

impl WizardInput for CheckersInput {
    type Value = usize;

    fn flag_value(&self) -> Option<Result<usize, String>> {
        self.flag.map(|value| Ok(value.max(1)))
    }

    fn prompt(&self) -> Result<usize, String> {
        let value = Input::<usize>::new()
            .with_prompt("rclone --checkers")
            .default(CHECKERS_DEFAULT)
            .interact_text()
            .map_err(|err| format!("failed to read checkers: {err}"))?;
        Ok(value.max(1))
    }

    fn non_interactive_fallback(&self) -> Result<usize, String> {
        Ok(CHECKERS_DEFAULT)
    }
}

/// Resolves rclone's `--tpslimit` (transactions/sec ceiling across
/// transfers and checkers combined). Unlike `TransfersInput`/
/// `CheckersInput`, "no cap" is itself a legitimate, distinct value here
/// (rclone's own behavior when the flag is omitted), not just "use a
/// baked-in default" -- so this resolves to `Option<usize>`, and both the
/// empty-prompt and non-interactive paths resolve to `None` (ADR-0108,
/// reverting ADR-0106's hardcoded `10` default: a single default rate
/// ceiling has been shown wrong in both directions -- too loose for B2,
/// too tight for Scaleway -- so the safer default is no cap at all).
struct TpslimitInput {
    flag: Option<usize>,
}

impl WizardInput for TpslimitInput {
    type Value = Option<usize>;

    fn flag_value(&self) -> Option<Result<Option<usize>, String>> {
        self.flag.map(|value| Ok(Some(value)))
    }

    fn prompt(&self) -> Result<Option<usize>, String> {
        let value = Input::<String>::new()
            .with_prompt("rclone --tpslimit (blank = no cap)")
            .allow_empty(true)
            .interact_text()
            .map_err(|err| format!("failed to read tpslimit: {err}"))?;
        let trimmed = value.trim();
        if trimmed.is_empty() {
            return Ok(None);
        }
        trimmed
            .parse::<usize>()
            .map(Some)
            .map_err(|_| format!("'{trimmed}' is not a valid tpslimit"))
    }

    fn non_interactive_fallback(&self) -> Result<Option<usize>, String> {
        Ok(None)
    }
}

fn default_local_output() -> PathBuf {
    std::env::temp_dir().join("pigeon-job")
}

/// Where this run's rclone log (also serving as this job's report) and
/// transcript are written -- unlike every other job, not a staging area
/// for transferred data, since rclone transfers directly source ->
/// destination with no pigeon-side staging.
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

/// Entry point for `pigeon job run import` (ADR-0101).
#[allow(clippy::too_many_arguments)]
pub fn dispatch(
    source: Option<String>,
    destination: Option<String>,
    local_output: Option<PathBuf>,
    report_bucket: Option<String>,
    transfers: Option<usize>,
    checkers: Option<usize>,
    tpslimit: Option<usize>,
    job_name: &'static str,
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
        source,
        destination,
        local_output,
        report_bucket,
        transfers,
        checkers,
        tpslimit,
        job_name,
        yes,
    ))
}

#[allow(clippy::too_many_arguments)]
async fn dispatch_async(
    source: Option<String>,
    destination: Option<String>,
    local_output: Option<PathBuf>,
    report_bucket: Option<String>,
    transfers: Option<usize>,
    checkers: Option<usize>,
    tpslimit: Option<usize>,
    job_name: &'static str,
    yes: bool,
) -> i32 {
    // Held for this whole async fn's lifetime -- every early `return
    // fail(...)` below drops it, aborting the sampling task automatically
    // (ADR-0073).
    let _sampler =
        crate::observability::resources::ResourceSampler::spawn(std::time::Duration::from_secs(5));

    // Checked before any prompts, mirroring `pull_transform::wizard`'s
    // `check_ffmpeg_available()` call -- fail immediately on a missing
    // binary, not partway through a long-running `rclone copy` subprocess.
    if let Err(err) = worker::check_rclone_available().await {
        return fail(err);
    }

    let keyring_store_path = match Store::default_path() {
        Ok(path) => path,
        Err(err) => return fail(err),
    };
    let keyring_store = match Store::load(&keyring_store_path) {
        Ok(store) => store,
        Err(err) => return fail(err),
    };

    let source = match (SourceInput { flag: source }).resolve() {
        Ok(value) => value,
        Err(err) => return fail(err),
    };
    let destination = match (DestinationInput { flag: destination }).resolve() {
        Ok(value) => value,
        Err(err) => return fail(err),
    };
    let local_output = match (LocalOutputInput { flag: local_output }).resolve() {
        Ok(value) => value,
        Err(err) => return fail(err),
    };
    let transfers = match (TransfersInput { flag: transfers }).resolve() {
        Ok(value) => value,
        Err(err) => return fail(err),
    };
    let checkers = match (CheckersInput { flag: checkers }).resolve() {
        Ok(value) => value,
        Err(err) => return fail(err),
    };
    let tpslimit = match (TpslimitInput { flag: tpslimit }).resolve() {
        Ok(value) => value,
        Err(err) => return fail(err),
    };
    let (report_bucket_config, report_secret) =
        match report_upload::resolve(report_bucket, &keyring_store) {
            Ok(value) => value,
            Err(err) => return fail(err),
        };

    println!("Source:      {source}");
    println!("Destination: {destination}");

    match (ConfirmInput { yes }).resolve() {
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

    let job = ImportJob {
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
        report_upload::say_error(&transcript, "simulated import failure");
        let contents = std::fs::read_to_string(&transcript_path).unwrap();
        assert!(!contents.is_empty());
        assert!(contents.contains("simulated import failure"));
    }

    #[test]
    fn transfers_input_flag_value_overrides_the_default() {
        let input = TransfersInput { flag: Some(32) };
        assert_eq!(input.flag_value(), Some(Ok(32)));
    }

    #[test]
    fn transfers_input_falls_back_to_a_default_when_not_interactive() {
        let input = TransfersInput { flag: None };
        assert_eq!(input.non_interactive_fallback(), Ok(TRANSFERS_DEFAULT));
    }

    #[test]
    fn checkers_input_flag_value_overrides_the_default() {
        let input = CheckersInput { flag: Some(64) };
        assert_eq!(input.flag_value(), Some(Ok(64)));
    }

    #[test]
    fn checkers_input_falls_back_to_a_default_when_not_interactive() {
        let input = CheckersInput { flag: None };
        assert_eq!(input.non_interactive_fallback(), Ok(CHECKERS_DEFAULT));
    }

    #[test]
    fn tpslimit_input_flag_value_overrides_the_default() {
        let input = TpslimitInput { flag: Some(10) };
        assert_eq!(input.flag_value(), Some(Ok(Some(10))));
    }

    #[test]
    fn tpslimit_input_falls_back_to_no_cap_when_not_interactive() {
        let input = TpslimitInput { flag: None };
        assert_eq!(input.non_interactive_fallback(), Ok(None));
    }
}
