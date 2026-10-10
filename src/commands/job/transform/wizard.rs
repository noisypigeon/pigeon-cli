//! CLI dispatch for `pigeon job run transform` (ADR-0112). `SourcePathInput`/
//! `DestinationPathInput`/`TransfersInput`/`CheckersInput`/`TpslimitInput`
//! are structurally identical to `rclone::wizard`'s own versions of the same
//! prompts -- kept local rather than hoisted, since this is only the second
//! consumer of that shape, per this codebase's "duplicate until the third
//! consumer" precedent.

use std::path::PathBuf;

use dialoguer::{Input, Select, theme::ColorfulTheme};

use crate::commands::job::report_upload;
use crate::commands::job::shared_wizard::{ConfirmInput, CpuConcurrencyInput};
use crate::commands::keyring::store::Store;
use crate::commands::{FAILURE_EXIT_CODE, fail};
use crate::core::job::Job;
use crate::core::wizard::WizardInput;

use super::format::InputFileType;
use super::{TransformJob, media, worker};

/// Resolves `--input-file-type` -- mandatory, a small vetted menu
/// (`InputFileType::parse`), never falls through to an interactive prompt
/// on an invalid flag value (`WizardInput::flag_value`'s own contract).
struct InputFileTypeInput {
    flag: Option<String>,
}

impl WizardInput for InputFileTypeInput {
    type Value = InputFileType;

    fn flag_value(&self) -> Option<Result<InputFileType, String>> {
        self.flag.as_deref().map(InputFileType::parse)
    }

    fn prompt(&self) -> Result<InputFileType, String> {
        let options = ["png", "jpeg", "heic"];
        let selected = Select::with_theme(&ColorfulTheme::default())
            .with_prompt("Input file type")
            .items(options)
            .default(0)
            .interact()
            .map_err(|err| format!("failed to read input file type: {err}"))?;
        InputFileType::parse(options[selected])
    }

    fn non_interactive_fallback(&self) -> Result<InputFileType, String> {
        Err("--input-file-type is required when not running interactively".to_string())
    }
}

/// Resolves the rclone source path, e.g. `source:png/`. A plain string, not
/// a pigeon bucket-config alias -- this job never looks it up against the
/// keyring (ADR-0101, reused by ADR-0112).
struct SourcePathInput {
    flag: Option<String>,
}

impl WizardInput for SourcePathInput {
    type Value = String;

    fn flag_value(&self) -> Option<Result<String, String>> {
        self.flag.clone().map(Ok)
    }

    fn prompt(&self) -> Result<String, String> {
        Input::<String>::new()
            .with_prompt("rclone source path (e.g. 'source:png/')")
            .interact_text()
            .map_err(|err| format!("failed to read source path: {err}"))
    }

    fn non_interactive_fallback(&self) -> Result<String, String> {
        Err("--source-path is required when not running interactively".to_string())
    }
}

/// Resolves the rclone destination path, e.g. `destination:jpg/`. Same
/// shape as `SourcePathInput`.
struct DestinationPathInput {
    flag: Option<String>,
}

impl WizardInput for DestinationPathInput {
    type Value = String;

    fn flag_value(&self) -> Option<Result<String, String>> {
        self.flag.clone().map(Ok)
    }

    fn prompt(&self) -> Result<String, String> {
        Input::<String>::new()
            .with_prompt("rclone destination path (e.g. 'destination:jpg/')")
            .interact_text()
            .map_err(|err| format!("failed to read destination path: {err}"))
    }

    fn non_interactive_fallback(&self) -> Result<String, String> {
        Err("--destination-path is required when not running interactively".to_string())
    }
}

const TRANSFERS_DEFAULT: usize = 8;

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

const CHECKERS_DEFAULT: usize = 16;

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

/// "No cap" is itself a legitimate, distinct value here, not just "use a
/// baked-in default" -- same reasoning as `rclone::wizard`'s own
/// `TpslimitInput`.
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
            .with_prompt("Local directory this run stages source/result/bookkeeping under")
            .default(default.display().to_string())
            .interact_text()
            .map_err(|err| format!("failed to read local output directory: {err}"))?;
        Ok(PathBuf::from(value))
    }

    fn non_interactive_fallback(&self) -> Result<PathBuf, String> {
        Ok(default_local_output())
    }
}

/// Confirms `rclone` is on `PATH` -- same shape as `rclone::worker`'s own
/// check, duplicated rather than reused since that function lives in a
/// private sibling module this job has no access path to.
async fn check_rclone_available() -> Result<(), String> {
    tokio::process::Command::new("rclone")
        .arg("version")
        .output()
        .await
        .map_err(|_| {
            "'rclone' was not found on PATH -- required for 'pigeon job run transform' \
             to pull/push data"
                .to_string()
        })?;
    Ok(())
}

/// Entry point for `pigeon job run transform` (ADR-0112).
#[allow(clippy::too_many_arguments)]
pub fn dispatch(
    input_file_type: Option<String>,
    source_path: Option<String>,
    destination_path: Option<String>,
    local_output: Option<PathBuf>,
    concurrency: Option<usize>,
    transfers: Option<usize>,
    checkers: Option<usize>,
    tpslimit: Option<usize>,
    report_bucket: Option<String>,
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
        input_file_type,
        source_path,
        destination_path,
        local_output,
        concurrency,
        transfers,
        checkers,
        tpslimit,
        report_bucket,
        job_name,
        yes,
    ))
}

#[allow(clippy::too_many_arguments)]
async fn dispatch_async(
    input_file_type: Option<String>,
    source_path: Option<String>,
    destination_path: Option<String>,
    local_output: Option<PathBuf>,
    concurrency: Option<usize>,
    transfers: Option<usize>,
    checkers: Option<usize>,
    tpslimit: Option<usize>,
    report_bucket: Option<String>,
    job_name: &'static str,
    yes: bool,
) -> i32 {
    // Held for this whole async fn's lifetime, same discipline as every
    // other job's dispatch_async (ADR-0073).
    let _sampler =
        crate::observability::resources::ResourceSampler::spawn(std::time::Duration::from_secs(5));

    // Checked before any prompts, mirroring `rclone::worker`'s/
    // `pull_transform::media`'s own preflight checks -- fail immediately on
    // a missing binary, not partway through a long-running pipeline.
    if let Err(err) = check_rclone_available().await {
        return fail(err);
    }
    if let Err(err) = media::check_ffmpeg_available().await {
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

    let input_file_type = match (InputFileTypeInput {
        flag: input_file_type,
    })
    .resolve()
    {
        Ok(value) => value,
        Err(err) => return fail(err),
    };
    let source_path = match (SourcePathInput { flag: source_path }).resolve() {
        Ok(value) => value,
        Err(err) => return fail(err),
    };
    let destination_path = match (DestinationPathInput {
        flag: destination_path,
    })
    .resolve()
    {
        Ok(value) => value,
        Err(err) => return fail(err),
    };
    let local_output = match (LocalOutputInput { flag: local_output }).resolve() {
        Ok(value) => value,
        Err(err) => return fail(err),
    };
    let concurrency = match (CpuConcurrencyInput { flag: concurrency }).resolve() {
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

    println!("Input file type: {input_file_type}");
    println!("Source:          {source_path}");
    println!("Destination:     {destination_path}");

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

    let job = TransformJob {
        source_path,
        destination_path,
        local_output: local_output.clone(),
        input_file_type,
        transfers,
        checkers,
        tpslimit,
        run_id,
    };
    let plan = match job.gather().await {
        Ok(plan) => plan,
        Err(err) => return fail(err),
    };

    let (exit_code, report_path) = match job.run(plan, concurrency, 1).await {
        Ok(summary) => {
            let transcoded = summary
                .outcomes
                .iter()
                .filter(|outcome| outcome.outcome == worker::Outcome::Transcoded)
                .count();
            let copied_through = summary
                .outcomes
                .iter()
                .filter(|outcome| outcome.outcome == worker::Outcome::CopiedThrough)
                .count();
            let failed = summary
                .outcomes
                .iter()
                .filter(|outcome| outcome.outcome == worker::Outcome::Failed)
                .count();
            let message = format!(
                "Pulled {} file(s); {transcoded} transcoded, {copied_through} copied through, \
                 {failed} failed; pushed {} file(s).",
                summary.pulled, summary.pushed
            );
            report_upload::say(&transcript, message);
            let exit_code = if summary.failed { FAILURE_EXIT_CODE } else { 0 };
            let report_path = worker::write_report(&local_output, &summary).unwrap_or_else(|err| {
                tracing::warn!(error = %err, "failed to write report");
                local_output.join("transform-report.txt")
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
    fn input_file_type_input_flag_value_parses_a_valid_value() {
        let input = InputFileTypeInput {
            flag: Some("png".to_string()),
        };
        assert_eq!(input.flag_value(), Some(Ok(InputFileType::Png)));
    }

    #[test]
    fn input_file_type_input_flag_value_surfaces_an_invalid_value_immediately() {
        let input = InputFileTypeInput {
            flag: Some("gif".to_string()),
        };
        assert!(input.flag_value().unwrap().is_err());
    }

    #[test]
    fn input_file_type_input_non_interactive_fallback_errors() {
        let input = InputFileTypeInput { flag: None };
        assert!(input.non_interactive_fallback().is_err());
    }

    #[test]
    fn source_path_input_non_interactive_fallback_errors() {
        let input = SourcePathInput { flag: None };
        assert_eq!(
            input.non_interactive_fallback(),
            Err("--source-path is required when not running interactively".to_string())
        );
    }

    #[test]
    fn destination_path_input_non_interactive_fallback_errors() {
        let input = DestinationPathInput { flag: None };
        assert_eq!(
            input.non_interactive_fallback(),
            Err("--destination-path is required when not running interactively".to_string())
        );
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

    #[test]
    fn default_local_output_is_under_the_os_temp_dir() {
        let path = default_local_output();
        assert!(path.starts_with(std::env::temp_dir()));
        assert_eq!(path.file_name().unwrap(), "pigeon-job");
    }

    #[test]
    fn transcript_contains_the_error_message_after_a_post_creation_failure() {
        let dir = tempfile::tempdir().unwrap();
        let (transcript, transcript_path) = report_upload::new_transcript(dir.path()).unwrap();
        report_upload::say_error(&transcript, "simulated transform failure");
        let contents = std::fs::read_to_string(&transcript_path).unwrap();
        assert!(!contents.is_empty());
        assert!(contents.contains("simulated transform failure"));
    }
}
