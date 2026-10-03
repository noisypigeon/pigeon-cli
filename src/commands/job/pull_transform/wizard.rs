use std::collections::HashSet;
use std::fs;
use std::io::IsTerminal;
use std::path::{Path, PathBuf};

use dialoguer::{Confirm, Input, MultiSelect, Select, theme::ColorfulTheme};

use crate::commands::FAILURE_EXIT_CODE;
use crate::commands::job::shared_wizard::{
    ConfirmInput, CpuConcurrencyInput, EncryptionKeyInput, UploadConcurrencyInput,
    UploadTargetInput,
};
use crate::commands::keyring::store::Store;
use crate::core::crypto::Aes256GcmSivEncryptor;
use crate::core::job::Job;
use crate::core::keyring::credentials;
use crate::core::wizard::WizardInput;

use super::manifest::{self, PROCESSED_FILE_NAME, PullTask};
use super::media::{self, TranscodeTargets, check_ffmpeg_available};
use super::worker;
use super::{PullTransformJob, TypeSummary};

/// Resolves which bucket-config to pull from -- mandatory (unlike
/// `UploadTargetInput`'s optional upload target), since there's no sane
/// default source for this job the way `email_sync`'s local-output has one.
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

/// Same shape as `email_sync::wizard`'s `LocalOutputInput` -- `--local-output`
/// if given, an editable prompt on a TTY, that same default silently
/// otherwise.
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

/// Prints the pre-run per-extension type summary table (ADR-0074 §3) --
/// reflects only pending (not yet checkpointed) objects, and doesn't yet
/// know what's inside any zip (unexpanded, shown as its own `zip` row).
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

/// Resolves which file extensions to pull/transform/upload this run
/// (ADR-0077): `--file-types` if given (the literal `none` maps to the
/// `"(none)"` extensionless bucket, matching `TypeSummary`'s own sentinel);
/// an interactive `MultiSelect` over the pending-summary table if omitted
/// and stdin is a terminal, everything pre-checked so hitting Enter
/// reproduces today's "pull everything" behavior exactly; every extension
/// seen in the manifest otherwise (same default, non-interactively).
struct FileTypesInput<'a> {
    flag: Option<Vec<String>>,
    available: &'a [TypeSummary],
}

impl WizardInput for FileTypesInput<'_> {
    type Value = HashSet<String>;

    fn flag_value(&self) -> Option<Result<HashSet<String>, String>> {
        self.flag.as_ref().map(|values| {
            Ok(values
                .iter()
                .map(|value| {
                    if value.eq_ignore_ascii_case("none") {
                        "(none)".to_string()
                    } else {
                        value.to_ascii_lowercase()
                    }
                })
                .collect())
        })
    }

    fn prompt(&self) -> Result<HashSet<String>, String> {
        if self.available.is_empty() {
            return Ok(HashSet::new());
        }
        let labels: Vec<String> = self
            .available
            .iter()
            .map(|summary| {
                format!(
                    "{} ({}, {})",
                    summary.extension,
                    summary.count,
                    format_bytes(summary.total_bytes)
                )
            })
            .collect();
        let defaults = vec![true; labels.len()];
        let selected = MultiSelect::with_theme(&ColorfulTheme::default())
            .with_prompt("Select file types to pull/transform/upload")
            .items(&labels)
            .defaults(&defaults)
            .interact()
            .map_err(|err| format!("failed to read file-type selection: {err}"))?;
        Ok(selected
            .into_iter()
            .map(|index| self.available[index].extension.clone())
            .collect())
    }

    fn non_interactive_fallback(&self) -> Result<HashSet<String>, String> {
        Ok(self
            .available
            .iter()
            .map(|summary| summary.extension.clone())
            .collect())
    }
}

/// Resolves which pending zip objects get expanded+transformed this run
/// (ADR-0077); every other pending zip is uploaded as-is, untouched.
/// `--expand-zips` if given; an interactive `MultiSelect` over just the
/// pending zips if omitted and stdin is a terminal, everything pre-checked
/// so hitting Enter reproduces today's "expand every zip" behavior exactly;
/// every pending zip otherwise (same default, non-interactively).
struct ZipHandlingInput<'a> {
    flag: Option<Vec<String>>,
    zip_tasks: &'a [&'a PullTask],
}

impl WizardInput for ZipHandlingInput<'_> {
    type Value = HashSet<String>;

    fn flag_value(&self) -> Option<Result<HashSet<String>, String>> {
        self.flag
            .as_ref()
            .map(|keys| Ok(keys.iter().cloned().collect()))
    }

    fn prompt(&self) -> Result<HashSet<String>, String> {
        let labels: Vec<String> = self
            .zip_tasks
            .iter()
            .map(|task| format!("{} ({})", task.key, format_bytes(task.size)))
            .collect();
        let defaults = vec![true; labels.len()];
        let selected = MultiSelect::with_theme(&ColorfulTheme::default())
            .with_prompt(
                "Select zip files to expand and transform (unselected zips upload as-is, untouched)",
            )
            .items(&labels)
            .defaults(&defaults)
            .interact()
            .map_err(|err| format!("failed to read zip-handling selection: {err}"))?;
        Ok(selected
            .into_iter()
            .map(|index| self.zip_tasks[index].key.clone())
            .collect())
    }

    fn non_interactive_fallback(&self) -> Result<HashSet<String>, String> {
        Ok(self.zip_tasks.iter().map(|task| task.key.clone()).collect())
    }
}

fn select_image_format(current: media::ImageFormat) -> Result<media::ImageFormat, String> {
    let options = media::ImageFormat::all();
    let labels: Vec<String> = options.iter().map(|option| option.to_string()).collect();
    let default_index = options
        .iter()
        .position(|option| *option == current)
        .unwrap_or(0);
    let index = Select::with_theme(&ColorfulTheme::default())
        .with_prompt("Image (photo/screenshot) target")
        .items(&labels)
        .default(default_index)
        .interact()
        .map_err(|err| format!("failed to read image format selection: {err}"))?;
    Ok(options[index])
}

fn select_video_format(current: media::VideoFormat) -> Result<media::VideoFormat, String> {
    let options = media::VideoFormat::all();
    let labels: Vec<String> = options.iter().map(|option| option.to_string()).collect();
    let default_index = options
        .iter()
        .position(|option| *option == current)
        .unwrap_or(0);
    let index = Select::with_theme(&ColorfulTheme::default())
        .with_prompt("Video target")
        .items(&labels)
        .default(default_index)
        .interact()
        .map_err(|err| format!("failed to read video format selection: {err}"))?;
    Ok(options[index])
}

fn select_audio_format(current: media::AudioFormat) -> Result<media::AudioFormat, String> {
    let options = media::AudioFormat::all();
    let labels: Vec<String> = options.iter().map(|option| option.to_string()).collect();
    let default_index = options
        .iter()
        .position(|option| *option == current)
        .unwrap_or(0);
    let index = Select::with_theme(&ColorfulTheme::default())
        .with_prompt("Audio target")
        .items(&labels)
        .default(default_index)
        .interact()
        .map_err(|err| format!("failed to read audio format selection: {err}"))?;
    Ok(options[index])
}

/// Resolves the media-transcoding mapping for this run (ADR-0077), never
/// persisted. Each of the three per-category flags always wins outright and
/// is validated independently; if *none* of them was passed, and only then,
/// a TTY is shown the resolved default mapping and asked once whether to
/// adapt it at all -- declining (or non-interactive with no flags) keeps
/// the exact pre-ADR-0077 mapping with zero prompts.
fn resolve_transcode_targets(
    image_flag: Option<String>,
    video_flag: Option<String>,
    audio_flag: Option<String>,
) -> Result<TranscodeTargets, String> {
    let any_flag = image_flag.is_some() || video_flag.is_some() || audio_flag.is_some();
    let mut targets = TranscodeTargets::default();
    if let Some(value) = image_flag {
        targets.image = media::ImageFormat::parse(&value)?;
    }
    if let Some(value) = video_flag {
        targets.video = media::VideoFormat::parse(&value)?;
    }
    if let Some(value) = audio_flag {
        targets.audio = media::AudioFormat::parse(&value)?;
    }
    if any_flag || !std::io::stdin().is_terminal() {
        return Ok(targets);
    }

    println!("Media transcoding mapping:");
    println!("  Image (photo/screenshot) -> {}", targets.image);
    println!("  Video                    -> {}", targets.video);
    println!("  Audio                    -> {}", targets.audio);
    let adapt = Confirm::with_theme(&ColorfulTheme::default())
        .with_prompt("Adapt this before running?")
        .default(false)
        .interact()
        .map_err(|err| format!("failed to read confirmation: {err}"))?;
    if !adapt {
        return Ok(targets);
    }

    targets.image = select_image_format(targets.image)?;
    targets.video = select_video_format(targets.video)?;
    targets.audio = select_audio_format(targets.audio)?;
    Ok(targets)
}

/// Entry point for `pigeon job run pull-transform` (ADR-0074).
#[allow(clippy::too_many_arguments)]
pub fn dispatch(
    source_bucket: Option<String>,
    local_output: Option<PathBuf>,
    remote_output: Option<String>,
    encryption_key: Option<String>,
    file_types: Option<Vec<String>>,
    expand_zips: Option<Vec<String>>,
    image_format: Option<String>,
    video_format: Option<String>,
    audio_format: Option<String>,
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
        encryption_key,
        file_types,
        expand_zips,
        image_format,
        video_format,
        audio_format,
        concurrency,
        upload_concurrency,
        upload_only,
        yes,
    ))
}

#[allow(clippy::too_many_arguments)]
async fn dispatch_async(
    source_bucket: Option<String>,
    local_output: Option<PathBuf>,
    remote_output: Option<String>,
    encryption_key: Option<String>,
    file_types: Option<Vec<String>>,
    expand_zips: Option<Vec<String>>,
    image_format: Option<String>,
    video_format: Option<String>,
    audio_format: Option<String>,
    concurrency: Option<usize>,
    upload_concurrency: Option<usize>,
    upload_only: bool,
    yes: bool,
) -> i32 {
    // Held for this whole async fn's lifetime -- every early `return
    // fail(...)` below drops it, aborting the sampling task automatically
    // (ADR-0073).
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
            encryption_key,
            upload_concurrency,
            yes,
            &keyring_store,
        )
        .await;
    }

    // Checked once, up front: a missing ffmpeg/ffprobe fails the whole job
    // immediately with one clear error, instead of failing per-file deep
    // into a long run (ADR-0074 §4). Skipped in `--upload-only` mode above
    // -- that mode never recodes anything, so ffmpeg isn't needed.
    if let Err(err) = check_ffmpeg_available().await {
        return fail(err);
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

    let mut job = PullTransformJob {
        source_bucket: source_bucket_config,
        source_secret,
        local_output,
        remote: None,
        encryptor: None,
        allowed_extensions: HashSet::new(),
        expand_zip_keys: HashSet::new(),
        transcode_targets: TranscodeTargets::default(),
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

    job.allowed_extensions = match (FileTypesInput {
        flag: file_types,
        available: &plan.type_summary,
    })
    .resolve()
    {
        Ok(set) => set,
        Err(err) => return fail(err),
    };

    let zip_tasks: Vec<&PullTask> = plan
        .tasks
        .iter()
        .filter(|task| manifest::extension_of(&task.key) == "zip")
        .collect();
    job.expand_zip_keys = if zip_tasks.is_empty() {
        HashSet::new()
    } else {
        match (ZipHandlingInput {
            flag: expand_zips,
            zip_tasks: &zip_tasks,
        })
        .resolve()
        {
            Ok(set) => set,
            Err(err) => return fail(err),
        }
    };

    job.transcode_targets =
        match resolve_transcode_targets(image_format, video_format, audio_format) {
            Ok(targets) => targets,
            Err(err) => return fail(err),
        };

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

    let resolved_encryption_key_alias = match (EncryptionKeyInput {
        flag: encryption_key,
        store: &keyring_store,
        uploading: job.remote.is_some(),
        bucket_default: job
            .remote
            .as_ref()
            .and_then(|(bc, _)| bc.encryption_key_alias.clone()),
    })
    .resolve()
    {
        Ok(alias) => alias,
        Err(err) => return fail(err),
    };
    job.encryptor = match resolved_encryption_key_alias {
        Some(alias) => {
            let key_hex = match credentials::get_secret(&alias) {
                Ok(secret) => secret,
                Err(err) => return fail(err),
            };
            match Aes256GcmSivEncryptor::from_hex_key(&key_hex) {
                Ok(encryptor) => Some(encryptor),
                Err(err) => return fail(err),
            }
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

    match job.run(plan, concurrency, upload_concurrency).await {
        Ok(summary) => {
            println!(
                "Processed {} file(s), {} failed ({} download, {} archive, {} classify, {} placement), {} skipped (type not selected), {} duplicate(s) skipped, {} recoded, {} kept as original (recode did not verify), {} uploaded, {} unchanged, {} upload failed.",
                summary.processed,
                summary.failed,
                summary.failure_breakdown.download,
                summary.failure_breakdown.archive,
                summary.failure_breakdown.classify,
                summary.failure_breakdown.placement,
                summary.skipped_type,
                summary.duplicates_skipped,
                summary.recoded,
                summary.recode_fallback_to_original,
                summary.uploaded,
                summary.unchanged,
                summary.upload_failed
            );
            crate::observability::metrics::record_job_summary(
                "pull-transform",
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

/// Whether `local_output` holds a completed prior pull-transform run that
/// `--upload-only` can resume uploading from: its `.processed` checkpoint
/// must exist directly under `local_output` (not under `.staging/` --
/// unlike `dedupe`/`sort`, this job never adopted that split) and at least
/// one placed-content subdirectory (`local_output/<extension>/...`, where
/// every real file lives) must exist besides `.staging` itself (ADR-0090).
fn upload_only_preflight_ok(local_output: &Path) -> bool {
    if !local_output.join(PROCESSED_FILE_NAME).exists() {
        return false;
    }
    fs::read_dir(local_output)
        .map(|entries| {
            entries
                .filter_map(|entry| entry.ok())
                .any(|entry| entry.file_name() != ".staging" && entry.path().is_dir())
        })
        .unwrap_or(false)
}

/// `--upload-only` branch of `dispatch_async` (ADR-0090): resumes uploading
/// an already-completed local pull-transform run, skipping the bucket
/// listing/download/classify/recode/placement phases -- and the source
/// bucket credentials and `ffmpeg`/`ffprobe` check they'd otherwise need --
/// entirely.
async fn dispatch_upload_only(
    local_output: Option<PathBuf>,
    remote_output: Option<String>,
    encryption_key: Option<String>,
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
            "no completed pull-transform run found under {}; run without --upload-only first",
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

    let resolved_encryption_key_alias = match (EncryptionKeyInput {
        flag: encryption_key,
        store: keyring_store,
        uploading: true,
        bucket_default: remote_bucket_config.encryption_key_alias.clone(),
    })
    .resolve()
    {
        Ok(alias) => alias,
        Err(err) => return fail(err),
    };
    let encryptor = match resolved_encryption_key_alias {
        Some(alias) => {
            let key_hex = match credentials::get_secret(&alias) {
                Ok(secret) => secret,
                Err(err) => return fail(err),
            };
            match Aes256GcmSivEncryptor::from_hex_key(&key_hex) {
                Ok(encryptor) => Some(encryptor),
                Err(err) => return fail(err),
            }
        }
        None => None,
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
        encryptor.as_ref(),
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
        fs::create_dir_all(dir.path().join("jpg")).unwrap();

        assert!(!upload_only_preflight_ok(dir.path()));
    }

    #[test]
    fn upload_only_preflight_fails_with_no_placed_content_subdirectory() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join(PROCESSED_FILE_NAME), b"a.jpg\n").unwrap();
        fs::create_dir_all(dir.path().join(".staging")).unwrap();

        assert!(!upload_only_preflight_ok(dir.path()));
    }

    #[test]
    fn upload_only_preflight_passes_for_a_completed_run() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join(PROCESSED_FILE_NAME), b"a.jpg\n").unwrap();
        fs::create_dir_all(dir.path().join(".staging")).unwrap();
        fs::create_dir_all(dir.path().join("jpg")).unwrap();

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

    fn summary(extension: &str) -> TypeSummary {
        TypeSummary {
            extension: extension.to_string(),
            count: 1,
            total_bytes: 100,
        }
    }

    #[test]
    fn file_types_input_flag_maps_the_literal_none_to_the_none_sentinel() {
        let input = FileTypesInput {
            flag: Some(vec!["JPG".to_string(), "none".to_string()]),
            available: &[],
        };
        let resolved = input.flag_value().unwrap().unwrap();
        assert!(resolved.contains("jpg"));
        assert!(resolved.contains("(none)"));
    }

    #[test]
    fn file_types_input_non_interactive_fallback_selects_every_available_extension() {
        let available = [summary("jpg"), summary("pdf"), summary("zip")];
        let input = FileTypesInput {
            flag: None,
            available: &available,
        };
        let resolved = input.non_interactive_fallback().unwrap();
        assert_eq!(resolved.len(), 3);
        assert!(resolved.contains("jpg"));
        assert!(resolved.contains("pdf"));
        assert!(resolved.contains("zip"));
    }

    fn zip_task(key: &str) -> PullTask {
        PullTask {
            key: key.to_string(),
            size: 1024,
        }
    }

    #[test]
    fn zip_handling_input_non_interactive_fallback_expands_every_pending_zip() {
        let a = zip_task("a.zip");
        let b = zip_task("b.zip");
        let tasks = [&a, &b];
        let input = ZipHandlingInput {
            flag: None,
            zip_tasks: &tasks,
        };
        let resolved = input.non_interactive_fallback().unwrap();
        assert_eq!(resolved.len(), 2);
        assert!(resolved.contains("a.zip"));
        assert!(resolved.contains("b.zip"));
    }

    #[test]
    fn zip_handling_input_flag_selects_only_the_named_keys() {
        let a = zip_task("a.zip");
        let tasks = [&a];
        let input = ZipHandlingInput {
            flag: Some(vec!["a.zip".to_string()]),
            zip_tasks: &tasks,
        };
        let resolved = input.flag_value().unwrap().unwrap();
        assert_eq!(resolved.len(), 1);
        assert!(resolved.contains("a.zip"));
    }

    #[test]
    fn resolve_transcode_targets_defaults_when_nothing_is_passed_non_interactively() {
        // cargo test's stdin isn't a TTY, so this exercises the
        // non-interactive branch deterministically.
        let targets = resolve_transcode_targets(None, None, None).unwrap();
        assert_eq!(targets.image, media::ImageFormat::Jpg);
        assert_eq!(targets.video, media::VideoFormat::Mp4);
        assert_eq!(targets.audio, media::AudioFormat::M4a);
    }

    #[test]
    fn resolve_transcode_targets_applies_explicit_flags_and_defaults_the_rest() {
        let targets = resolve_transcode_targets(Some("png".to_string()), None, None).unwrap();
        assert_eq!(targets.image, media::ImageFormat::Png);
        assert_eq!(targets.video, media::VideoFormat::Mp4);
        assert_eq!(targets.audio, media::AudioFormat::M4a);
    }

    #[test]
    fn resolve_transcode_targets_errors_on_an_unknown_format() {
        let result = resolve_transcode_targets(None, Some("betamax".to_string()), None);
        assert!(result.is_err());
    }
}
