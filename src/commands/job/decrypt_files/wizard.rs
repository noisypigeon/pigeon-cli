use std::path::PathBuf;

use dialoguer::Input;

use crate::commands::job::report_upload;
use crate::commands::job::shared_wizard::{ConfirmInput, CpuConcurrencyInput};
use crate::commands::keyring::store::Store;
use crate::commands::{FAILURE_EXIT_CODE, fail};
use crate::core::crypto::Aes256GcmSivEncryptor;
use crate::core::job::Job;
use crate::core::keyring::credentials;
use crate::core::wizard::WizardInput;

use super::DecryptFilesJob;

/// Resolves the directory containing `*.enc` files: `--input-dir` if given,
/// an interactive prompt if omitted and stdin is a terminal, a hard error
/// otherwise -- same required-with-no-safe-default shape as `IdentitiesInput`
/// (`email_sync::wizard`).
struct InputDirInput {
    flag: Option<PathBuf>,
}

impl WizardInput for InputDirInput {
    type Value = PathBuf;

    fn flag_value(&self) -> Option<Result<PathBuf, String>> {
        self.flag.clone().map(Ok)
    }

    fn prompt(&self) -> Result<PathBuf, String> {
        Input::<String>::new()
            .with_prompt("Input directory (containing .enc files)")
            .interact_text()
            .map(PathBuf::from)
            .map_err(|err| format!("failed to read input directory: {err}"))
    }

    fn non_interactive_fallback(&self) -> Result<PathBuf, String> {
        Err("--input-dir is required when not running interactively".to_string())
    }
}

/// Resolves the directory decrypted files are written under -- same shape
/// as `InputDirInput`.
struct OutputDirInput {
    flag: Option<PathBuf>,
}

impl WizardInput for OutputDirInput {
    type Value = PathBuf;

    fn flag_value(&self) -> Option<Result<PathBuf, String>> {
        self.flag.clone().map(Ok)
    }

    fn prompt(&self) -> Result<PathBuf, String> {
        Input::<String>::new()
            .with_prompt("Output directory (decrypted files written here)")
            .interact_text()
            .map(PathBuf::from)
            .map_err(|err| format!("failed to read output directory: {err}"))
    }

    fn non_interactive_fallback(&self) -> Result<PathBuf, String> {
        Err("--output-dir is required when not running interactively".to_string())
    }
}

/// Resolves which encryption key to decrypt with. Unlike
/// `email_sync::wizard::EncryptionKeyInput` (optional, `Option<String>`),
/// this one is mandatory -- decrypting is the entire point of this job, so
/// there's no "skip" case.
struct EncryptionKeyInput<'a> {
    flag: Option<String>,
    store: &'a Store,
}

impl WizardInput for EncryptionKeyInput<'_> {
    type Value = String;

    fn flag_value(&self) -> Option<Result<String, String>> {
        self.flag.clone().map(Ok)
    }

    fn prompt(&self) -> Result<String, String> {
        self.store
            .prompt_select_encryption_key()
            .map(|key| key.alias.clone())
    }

    fn non_interactive_fallback(&self) -> Result<String, String> {
        Err("--encryption-key is required when not running interactively".to_string())
    }
}

/// Entry point for `pigeon job run decrypt-files` (ADR-0028).
#[allow(clippy::too_many_arguments)]
pub fn dispatch(
    input_dir: Option<PathBuf>,
    output_dir: Option<PathBuf>,
    encryption_key: Option<String>,
    concurrency: Option<usize>,
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
        input_dir,
        output_dir,
        encryption_key,
        concurrency,
        report_bucket,
        job_name,
        yes,
    ))
}

#[allow(clippy::too_many_arguments)]
async fn dispatch_async(
    input_dir: Option<PathBuf>,
    output_dir: Option<PathBuf>,
    encryption_key: Option<String>,
    concurrency: Option<usize>,
    report_bucket: Option<String>,
    job_name: &'static str,
    yes: bool,
) -> i32 {
    // Held for this whole async fn's lifetime -- every early `return fail(...)`
    // below drops it, aborting the sampling task automatically (ADR-0073).
    let _sampler =
        crate::observability::resources::ResourceSampler::spawn(std::time::Duration::from_secs(5));

    let input_dir = match (InputDirInput { flag: input_dir }).resolve() {
        Ok(path) => path,
        Err(err) => return fail(err),
    };
    let output_dir = match (OutputDirInput { flag: output_dir }).resolve() {
        Ok(path) => path,
        Err(err) => return fail(err),
    };
    if let (Ok(a), Ok(b)) = (input_dir.canonicalize(), output_dir.canonicalize())
        && a == b
    {
        return fail("--input-dir and --output-dir must not be the same directory");
    }

    let keyring_store_path = match Store::default_path() {
        Ok(path) => path,
        Err(err) => return fail(err),
    };
    let keyring_store = match Store::load(&keyring_store_path) {
        Ok(store) => store,
        Err(err) => return fail(err),
    };
    let alias = match (EncryptionKeyInput {
        flag: encryption_key,
        store: &keyring_store,
    })
    .resolve()
    {
        Ok(alias) => alias,
        Err(err) => return fail(err),
    };
    let key_hex = match credentials::get_secret(&alias) {
        Ok(secret) => secret,
        Err(err) => return fail(err),
    };
    let encryptor = match Aes256GcmSivEncryptor::from_hex_key(&key_hex) {
        Ok(encryptor) => encryptor,
        Err(err) => return fail(err),
    };

    let job = DecryptFilesJob {
        input_dir,
        output_dir,
        encryptor,
    };
    let plan = match job.gather().await {
        Ok(plan) => plan,
        Err(err) => return fail(err),
    };
    if plan.is_empty() {
        println!("No encrypted files found in {}.", job.input_dir.display());
        return 0;
    }
    println!("{} encrypted file(s) found.", plan.len());

    let concurrency = match (CpuConcurrencyInput { flag: concurrency }).resolve() {
        Ok(value) => value,
        Err(err) => return fail(err),
    };
    let (report_bucket_config, report_secret) =
        match report_upload::resolve(report_bucket, &keyring_store) {
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

    let run_id = report_upload::generate_run_id();
    let run_prefix = report_upload::run_prefix(job_name, &run_id);
    let output_dir = job.output_dir.clone();
    let (transcript, transcript_path) = match report_upload::new_transcript(&output_dir) {
        Ok(value) => value,
        Err(err) => return fail(err),
    };

    let (exit_code, report_path) = match job
        .run(
            plan,
            concurrency,
            1, /* ignored -- DecryptFilesJob has no upload phase */
        )
        .await
    {
        Ok(summary) => {
            let message = format!(
                "Decrypted {} file(s), {} failed.",
                summary.decrypted, summary.failed
            );
            report_upload::say(&transcript, message);
            let exit_code = if summary.failed > 0 {
                FAILURE_EXIT_CODE
            } else {
                0
            };
            let report_path = report_upload::write_summary_report(&output_dir, job_name, &summary)
                .unwrap_or_else(|err| {
                    tracing::warn!(error = %err, "failed to write report");
                    output_dir.join(format!("{job_name}-report.txt"))
                });
            (exit_code, report_path)
        }
        Err(err) => {
            let report_path = report_upload::write_summary_report(&output_dir, job_name, &err)
                .unwrap_or_else(|_| output_dir.join(format!("{job_name}-report.txt")));
            (fail(err), report_path)
        }
    };
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
