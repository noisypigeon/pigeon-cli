//! CLI dispatch for `pigeon job run transform` (ADR-0112). `SourceBucketInput`/
//! `DestinationBucketInput`/`TransfersInput`/`CheckersInput`/`TpslimitInput`
//! are structurally identical to `rclone::wizard`'s own versions of the same
//! prompts -- kept local rather than hoisted, since this is only the second
//! consumer of that shape, per this codebase's "duplicate until the third
//! consumer" precedent.

use std::path::PathBuf;

use dialoguer::{Input, Select, theme::ColorfulTheme};

use crate::commands::job::report_upload;
use crate::commands::job::shared_wizard::{
    ConfirmInput, CpuConcurrencyInput, TpslimitInput, load_keyring, prepare_rclone,
};
use crate::commands::keyring::store::Store;
use crate::commands::{FAILURE_EXIT_CODE, fail};
use crate::core::job::Job;
use crate::core::rclone::config::RemoteTarget;
use crate::core::wizard::WizardInput;

use super::format::{InputFileType, VideoQuality};
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
        let options = ["png", "jpeg", "heic", "mov", "m4v", "mp4", "mp4-audio"];
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

/// Resolves `--video-quality` (ADR-0122) -- only consulted when
/// `input_file_type.is_video()`; `dispatch_async` skips calling this
/// entirely otherwise, so png/jpeg/heic runs are never prompted. Unlike
/// `InputFileTypeInput`, a missing value defaults to `Medium` rather than
/// hard-erroring on a non-interactive run -- a safe default exists, and
/// this flag isn't foundational to the run the way source/destination/type
/// are.
struct VideoQualityInput {
    flag: Option<String>,
}

impl WizardInput for VideoQualityInput {
    type Value = VideoQuality;

    fn flag_value(&self) -> Option<Result<VideoQuality, String>> {
        self.flag.as_deref().map(VideoQuality::parse)
    }

    fn prompt(&self) -> Result<VideoQuality, String> {
        let options = ["low", "medium", "high", "lossless"];
        let selected = Select::with_theme(&ColorfulTheme::default())
            .with_prompt("Video quality")
            .items(options)
            .default(1)
            .interact()
            .map_err(|err| format!("failed to read video quality: {err}"))?;
        VideoQuality::parse(options[selected])
    }

    fn non_interactive_fallback(&self) -> Result<VideoQuality, String> {
        Ok(VideoQuality::Medium)
    }
}

/// Resolves `--source`, a pigeon keyring `BucketConfig` alias optionally
/// composed with a prefix within it -- `source` is the whole bucket,
/// `source:mp4/` its `mp4/` directory (ADR-0138, restoring the addressing
/// ADR-0136 removed and ADR-0137 compounded). `rclone` only ever sees a
/// config file pigeon itself generates from the alias half, which is
/// validated against the keyring exactly as before.
///
/// `default_prefix` is `input_file_type.canonical_extension()`: the prefix used when
/// the flag names a bucket but no prefix (`--source source` with
/// `--input-file-type mp4` → `source:mp4/`). An explicitly-given prefix is
/// always kept verbatim. The **alias** half is never inferred -- that was
/// ADR-0137's mechanism, reversed by ADR-0138 because it assumed a
/// per-file-type bucket naming convention this deployment does not use.
struct SourceBucketInput<'a> {
    flag: Option<String>,
    store: &'a Store,
    default_prefix: &'a str,
}

impl WizardInput for SourceBucketInput<'_> {
    type Value = RemoteTarget;

    fn flag_value(&self) -> Option<Result<RemoteTarget, String>> {
        self.flag.as_deref().map(|value| {
            RemoteTarget::parse(value, self.store)?.with_default_prefix(self.default_prefix)
        })
    }

    fn prompt(&self) -> Result<RemoteTarget, String> {
        let bucket_config = self.store.prompt_select_bucket()?;
        RemoteTarget::bucket_root(&bucket_config.alias).with_default_prefix(self.default_prefix)
    }

    fn non_interactive_fallback(&self) -> Result<RemoteTarget, String> {
        Err("--source is required when not running interactively".to_string())
    }
}

/// Resolves `--destination`. Same shape as `SourceBucketInput`, composed
/// form included; `default_prefix` is `input_file_type.output_extension()`
/// (ADR-0138), so a `--destination destination` for an mp4 run addresses
/// `destination:mp4/`.
struct DestinationBucketInput<'a> {
    flag: Option<String>,
    store: &'a Store,
    default_prefix: &'a str,
}

impl WizardInput for DestinationBucketInput<'_> {
    type Value = RemoteTarget;

    fn flag_value(&self) -> Option<Result<RemoteTarget, String>> {
        self.flag.as_deref().map(|value| {
            RemoteTarget::parse(value, self.store)?.with_default_prefix(self.default_prefix)
        })
    }

    fn prompt(&self) -> Result<RemoteTarget, String> {
        let bucket_config = self.store.prompt_select_bucket()?;
        RemoteTarget::bucket_root(&bucket_config.alias).with_default_prefix(self.default_prefix)
    }

    fn non_interactive_fallback(&self) -> Result<RemoteTarget, String> {
        Err("--destination is required when not running interactively".to_string())
    }
}

const DOWNLOAD_TRANSFERS_DEFAULT: usize = 8;

struct DownloadTransfersInput {
    flag: Option<usize>,
}

impl WizardInput for DownloadTransfersInput {
    type Value = usize;

    fn flag_value(&self) -> Option<Result<usize, String>> {
        self.flag.map(|value| Ok(value.max(1)))
    }

    fn prompt(&self) -> Result<usize, String> {
        let value = Input::<usize>::new()
            .with_prompt("rclone --downloads-transfers (pull phase)")
            .default(DOWNLOAD_TRANSFERS_DEFAULT)
            .interact_text()
            .map_err(|err| format!("failed to read download transfers: {err}"))?;
        Ok(value.max(1))
    }

    fn non_interactive_fallback(&self) -> Result<usize, String> {
        Ok(DOWNLOAD_TRANSFERS_DEFAULT)
    }
}

const DOWNLOAD_CHECKERS_DEFAULT: usize = 16;

struct DownloadCheckersInput {
    flag: Option<usize>,
}

impl WizardInput for DownloadCheckersInput {
    type Value = usize;

    fn flag_value(&self) -> Option<Result<usize, String>> {
        self.flag.map(|value| Ok(value.max(1)))
    }

    fn prompt(&self) -> Result<usize, String> {
        let value = Input::<usize>::new()
            .with_prompt("rclone --download-checkers (pull phase)")
            .default(DOWNLOAD_CHECKERS_DEFAULT)
            .interact_text()
            .map_err(|err| format!("failed to read download checkers: {err}"))?;
        Ok(value.max(1))
    }

    fn non_interactive_fallback(&self) -> Result<usize, String> {
        Ok(DOWNLOAD_CHECKERS_DEFAULT)
    }
}

const UPLOAD_TRANSFERS_DEFAULT: usize = 8;

struct UploadTransfersInput {
    flag: Option<usize>,
}

impl WizardInput for UploadTransfersInput {
    type Value = usize;

    fn flag_value(&self) -> Option<Result<usize, String>> {
        self.flag.map(|value| Ok(value.max(1)))
    }

    fn prompt(&self) -> Result<usize, String> {
        let value = Input::<usize>::new()
            .with_prompt("concurrent push operations (upload phase)")
            .default(UPLOAD_TRANSFERS_DEFAULT)
            .interact_text()
            .map_err(|err| format!("failed to read upload transfers: {err}"))?;
        Ok(value.max(1))
    }

    fn non_interactive_fallback(&self) -> Result<usize, String> {
        Ok(UPLOAD_TRANSFERS_DEFAULT)
    }
}

struct MaxPendingDownloadsInput {
    flag: Option<usize>,
    concurrency: usize,
}

impl WizardInput for MaxPendingDownloadsInput {
    type Value = usize;

    fn flag_value(&self) -> Option<Result<usize, String>> {
        self.flag.map(|value| Ok(value.max(1)))
    }

    fn prompt(&self) -> Result<usize, String> {
        let default = self.concurrency * 4;
        let value = Input::<usize>::new()
            .with_prompt("max pending downloads before pause (blank = auto)")
            .default(default)
            .interact_text()
            .map_err(|err| format!("failed to read max pending downloads: {err}"))?;
        Ok(value.max(1))
    }

    fn non_interactive_fallback(&self) -> Result<usize, String> {
        Ok(self.concurrency * 4)
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

/// Entry point for `pigeon job run transform` (ADR-0112, video input kinds
/// added by ADR-0122, download/upload concurrency split and backpressure
/// added by ADR-0129).
#[allow(clippy::too_many_arguments)]
pub fn dispatch(
    input_file_type: Option<String>,
    video_quality: Option<String>,
    source: Option<String>,
    destination: Option<String>,
    local_output: Option<PathBuf>,
    concurrency: Option<usize>,
    download_transfers: Option<usize>,
    download_checkers: Option<usize>,
    upload_transfers: Option<usize>,
    max_pending_downloads: Option<usize>,
    tpslimit: Option<usize>,
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
        input_file_type,
        video_quality,
        source,
        destination,
        local_output,
        concurrency,
        download_transfers,
        download_checkers,
        upload_transfers,
        max_pending_downloads,
        tpslimit,
        report_bucket,
        job_name,
        non_interactive,
    ))
}

#[allow(clippy::too_many_arguments)]
async fn dispatch_async(
    input_file_type: Option<String>,
    video_quality: Option<String>,
    source: Option<String>,
    destination: Option<String>,
    local_output: Option<PathBuf>,
    concurrency: Option<usize>,
    download_transfers: Option<usize>,
    download_checkers: Option<usize>,
    upload_transfers: Option<usize>,
    max_pending_downloads: Option<usize>,
    tpslimit: Option<usize>,
    report_bucket: Option<String>,
    job_name: &'static str,
    non_interactive: bool,
) -> i32 {
    // Held for this whole async fn's lifetime, same discipline as every
    // other job's dispatch_async (ADR-0073).
    let _sampler =
        crate::observability::resources::ResourceSampler::spawn(std::time::Duration::from_secs(5));

    // Checked before any prompts -- fail immediately on a missing binary,
    // not partway through a long-running pipeline. `rclone`'s own
    // equivalent check now lives in `prepare_rclone` below (ADR-0145 §5).
    if let Err(err) = media::check_ffmpeg_available().await {
        return fail(err);
    }

    let keyring_store = match load_keyring() {
        Ok(store) => store,
        Err(err) => return fail(err),
    };
    if let Err(err) = prepare_rclone(&keyring_store).await {
        return fail(err);
    }

    let input_file_type = match (InputFileTypeInput {
        flag: input_file_type,
    })
    .resolve(non_interactive)
    {
        Ok(value) => value,
        Err(err) => return fail(err),
    };
    // Only consulted for a video input kind (ADR-0122) -- png/jpeg/heic runs
    // are never prompted for this and the value goes unused.
    let video_quality = if input_file_type.is_video() {
        match (VideoQualityInput {
            flag: video_quality,
        })
        .resolve(non_interactive)
        {
            Ok(value) => value,
            Err(err) => return fail(err),
        }
    } else {
        VideoQuality::Medium
    };
    let source_path = match (SourceBucketInput {
        flag: source,
        store: &keyring_store,
        default_prefix: input_file_type.canonical_extension(),
    })
    .resolve(non_interactive)
    {
        Ok(target) => target.address(),
        Err(err) => return fail(err),
    };
    let destination_path = match (DestinationBucketInput {
        flag: destination,
        store: &keyring_store,
        default_prefix: input_file_type.output_extension(),
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
    let concurrency = match (CpuConcurrencyInput { flag: concurrency }).resolve(non_interactive) {
        Ok(value) => value,
        Err(err) => return fail(err),
    };
    let download_transfers = match (DownloadTransfersInput {
        flag: download_transfers,
    })
    .resolve(non_interactive)
    {
        Ok(value) => value,
        Err(err) => return fail(err),
    };
    let download_checkers = match (DownloadCheckersInput {
        flag: download_checkers,
    })
    .resolve(non_interactive)
    {
        Ok(value) => value,
        Err(err) => return fail(err),
    };
    let upload_transfers = match (UploadTransfersInput {
        flag: upload_transfers,
    })
    .resolve(non_interactive)
    {
        Ok(value) => value,
        Err(err) => return fail(err),
    };
    let max_pending_downloads = match (MaxPendingDownloadsInput {
        flag: max_pending_downloads,
        concurrency,
    })
    .resolve(non_interactive)
    {
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

    println!("Input file type: {input_file_type}");
    if input_file_type.is_video() {
        println!("Video quality:   {video_quality}");
    }
    println!("Source:          {source_path}");
    println!("Destination:     {destination_path}");

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

    let job = TransformJob {
        source_path,
        destination_path,
        local_output: local_output.clone(),
        input_file_type,
        video_quality,
        download_transfers,
        download_checkers,
        tpslimit,
        max_pending_downloads,
        run_id,
    };
    let plan = match job.gather().await {
        Ok(plan) => plan,
        Err(err) => return fail(err),
    };

    let (exit_code, report_path) = match job.run(plan, concurrency, upload_transfers).await {
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
            let mut message = format!(
                "Pulled {} file(s); {transcoded} transcoded, {copied_through} copied through, \
                 {failed} failed; pushed {} file(s).",
                summary.pulled, summary.pushed
            );
            // ADR-0130: a run that correctly skipped a large already-done
            // tree used to read identically to one that found nothing, and a
            // failure gave no indication of which stage it came from.
            if summary.skipped_already_done > 0 || summary.skipped_apple_double > 0 {
                message.push_str(&format!(
                    " Skipped {} already done, {} AppleDouble sidecar(s).",
                    summary.skipped_already_done, summary.skipped_apple_double
                ));
            }
            if summary.failed {
                let f = summary.failures;
                message.push_str(&format!(
                    " Failures by stage: {} pull, {} transcode, {} place, {} push.",
                    f.pull, f.transcode, f.place, f.push
                ));
            }
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
    fn input_file_type_input_flag_value_parses_every_video_variant() {
        for (flag, expected) in [
            ("mov", InputFileType::Mov),
            ("m4v", InputFileType::M4v),
            ("mp4", InputFileType::Mp4),
        ] {
            let input = InputFileTypeInput {
                flag: Some(flag.to_string()),
            };
            assert_eq!(input.flag_value(), Some(Ok(expected)));
        }
    }

    #[test]
    fn video_quality_input_flag_value_parses_a_valid_value() {
        let input = VideoQualityInput {
            flag: Some("high".to_string()),
        };
        assert_eq!(input.flag_value(), Some(Ok(VideoQuality::High)));
    }

    #[test]
    fn video_quality_input_flag_value_surfaces_an_invalid_value_immediately() {
        let input = VideoQualityInput {
            flag: Some("ultra".to_string()),
        };
        assert!(input.flag_value().unwrap().is_err());
    }

    #[test]
    fn video_quality_input_non_interactive_fallback_defaults_to_medium() {
        let input = VideoQualityInput { flag: None };
        assert_eq!(input.non_interactive_fallback(), Ok(VideoQuality::Medium));
    }

    fn store_with_bucket(alias: &str) -> Store {
        let mut store = Store::default();
        store.push(crate::commands::keyring::store::Entry::Bucket(
            crate::commands::keyring::bucket::store::BucketConfig {
                alias: alias.to_string(),
                endpoint: "https://nyc3.digitaloceanspaces.com".to_string(),
                bucket: "my-bucket".to_string(),
                access_key_id: "AKID".to_string(),
                secret_key: "s3-secret-key".to_string(),
            },
        ));
        store
    }

    /// The exact regression ADR-0138 fixes: a bare bucket alias plus
    /// `--input-file-type mp4` must address that bucket's `mp4/` directory,
    /// not its root.
    #[test]
    fn source_bucket_input_defaults_the_prefix_to_the_input_extension() {
        let store = store_with_bucket("source");
        let input = SourceBucketInput {
            flag: Some("source".to_string()),
            store: &store,
            default_prefix: "mp4",
        };
        assert_eq!(
            input.flag_value().unwrap().unwrap().address(),
            "source:mp4/"
        );
    }

    #[test]
    fn source_bucket_input_keeps_an_explicit_prefix_verbatim() {
        let store = store_with_bucket("source");
        let input = SourceBucketInput {
            flag: Some("source:raw-video/".to_string()),
            store: &store,
            default_prefix: "mp4",
        };
        assert_eq!(
            input.flag_value().unwrap().unwrap().address(),
            "source:raw-video/"
        );
    }

    #[test]
    fn source_bucket_input_rejects_an_unknown_alias_in_the_composed_form() {
        let store = store_with_bucket("source");
        let input = SourceBucketInput {
            flag: Some("nope:mp4/".to_string()),
            store: &store,
            default_prefix: "mp4",
        };
        assert_eq!(
            input.flag_value(),
            Some(Err("no bucket-config named 'nope'".to_string()))
        );
    }

    /// ADR-0138 reverses ADR-0137's alias inference: with no `--source` at
    /// all there is no alias to guess, so a non-interactive run errors.
    #[test]
    fn source_bucket_input_non_interactive_fallback_errors_without_a_flag() {
        let store = store_with_bucket("source");
        let input = SourceBucketInput {
            flag: None,
            store: &store,
            default_prefix: "mp4",
        };
        assert_eq!(
            input.non_interactive_fallback(),
            Err("--source is required when not running interactively".to_string())
        );
    }

    #[test]
    fn destination_bucket_input_defaults_the_prefix_to_the_output_extension() {
        let store = store_with_bucket("destination");
        let input = DestinationBucketInput {
            flag: Some("destination".to_string()),
            store: &store,
            default_prefix: "mp4",
        };
        assert_eq!(
            input.flag_value().unwrap().unwrap().address(),
            "destination:mp4/"
        );
    }

    #[test]
    fn destination_bucket_input_keeps_an_explicit_prefix_verbatim() {
        let store = store_with_bucket("destination");
        let input = DestinationBucketInput {
            flag: Some("destination:archive/2026/".to_string()),
            store: &store,
            default_prefix: "mp4",
        };
        assert_eq!(
            input.flag_value().unwrap().unwrap().address(),
            "destination:archive/2026/"
        );
    }

    #[test]
    fn destination_bucket_input_non_interactive_fallback_errors_without_a_flag() {
        let store = store_with_bucket("destination");
        let input = DestinationBucketInput {
            flag: None,
            store: &store,
            default_prefix: "mp4",
        };
        assert_eq!(
            input.non_interactive_fallback(),
            Err("--destination is required when not running interactively".to_string())
        );
    }

    /// Source and destination can address the *same* bucket, distinguished
    /// only by prefix -- the layout ADR-0137's alias inference could not
    /// express, and which collided outright for video kinds (every one of
    /// them outputs `mp4`).
    #[test]
    fn one_bucket_can_serve_as_both_source_and_destination_via_differing_prefixes() {
        let store = store_with_bucket("media");
        let source = (SourceBucketInput {
            flag: Some("media".to_string()),
            store: &store,
            default_prefix: "mov",
        })
        .flag_value()
        .unwrap()
        .unwrap();
        let destination = (DestinationBucketInput {
            flag: Some("media".to_string()),
            store: &store,
            default_prefix: "mp4",
        })
        .flag_value()
        .unwrap()
        .unwrap();
        assert_eq!(source.address(), "media:mov/");
        assert_eq!(destination.address(), "media:mp4/");
        assert_ne!(source.address(), destination.address());
    }

    #[test]
    fn download_transfers_input_flag_value_overrides_the_default() {
        let input = DownloadTransfersInput { flag: Some(32) };
        assert_eq!(input.flag_value(), Some(Ok(32)));
    }

    #[test]
    fn download_transfers_input_falls_back_to_a_default_when_not_interactive() {
        let input = DownloadTransfersInput { flag: None };
        assert_eq!(
            input.non_interactive_fallback(),
            Ok(DOWNLOAD_TRANSFERS_DEFAULT)
        );
    }

    #[test]
    fn download_checkers_input_flag_value_overrides_the_default() {
        let input = DownloadCheckersInput { flag: Some(64) };
        assert_eq!(input.flag_value(), Some(Ok(64)));
    }

    #[test]
    fn download_checkers_input_falls_back_to_a_default_when_not_interactive() {
        let input = DownloadCheckersInput { flag: None };
        assert_eq!(
            input.non_interactive_fallback(),
            Ok(DOWNLOAD_CHECKERS_DEFAULT)
        );
    }

    #[test]
    fn upload_transfers_input_flag_value_overrides_the_default() {
        let input = UploadTransfersInput { flag: Some(16) };
        assert_eq!(input.flag_value(), Some(Ok(16)));
    }

    #[test]
    fn upload_transfers_input_falls_back_to_a_default_when_not_interactive() {
        let input = UploadTransfersInput { flag: None };
        assert_eq!(
            input.non_interactive_fallback(),
            Ok(UPLOAD_TRANSFERS_DEFAULT)
        );
    }

    #[test]
    fn max_pending_downloads_input_scales_fallback_with_concurrency() {
        let input = MaxPendingDownloadsInput {
            flag: None,
            concurrency: 4,
        };
        assert_eq!(input.non_interactive_fallback(), Ok(16));
        let input = MaxPendingDownloadsInput {
            flag: None,
            concurrency: 8,
        };
        assert_eq!(input.non_interactive_fallback(), Ok(32));
    }

    #[test]
    fn max_pending_downloads_input_flag_value_overrides_the_default() {
        let input = MaxPendingDownloadsInput {
            flag: Some(20),
            concurrency: 4,
        };
        assert_eq!(input.flag_value(), Some(Ok(20)));
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
