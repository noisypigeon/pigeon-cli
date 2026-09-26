use std::path::PathBuf;

use dialoguer::{Confirm, Input, theme::ColorfulTheme};

use crate::commands::FAILURE_EXIT_CODE;
use crate::commands::keyring::store::Store;
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

/// Resolves the decrypt concurrency: `--concurrency` if given, a plain
/// interactive prompt otherwise -- no time-estimate table (unlike
/// `email_sync::wizard::ConcurrencyInput`), since no throughput data exists
/// for this workload either (ADR-0021 §9's own reasoning applies here too).
struct ConcurrencyInput {
    flag: Option<usize>,
}

impl WizardInput for ConcurrencyInput {
    type Value = usize;

    fn flag_value(&self) -> Option<Result<usize, String>> {
        self.flag.map(|value| Ok(value.max(1)))
    }

    fn prompt(&self) -> Result<usize, String> {
        let value = Input::<usize>::new()
            .with_prompt("Concurrency")
            .default(4)
            .interact_text()
            .map_err(|err| format!("failed to read concurrency: {err}"))?;
        Ok(value.max(1))
    }

    fn non_interactive_fallback(&self) -> Result<usize, String> {
        Err("--concurrency is required when not running interactively".to_string())
    }
}

/// The final "proceed?" gate, identical to `email_sync::wizard::ConfirmInput`.
struct ConfirmInput {
    yes: bool,
}

impl WizardInput for ConfirmInput {
    type Value = bool;

    fn flag_value(&self) -> Option<Result<bool, String>> {
        self.yes.then_some(Ok(true))
    }

    fn prompt(&self) -> Result<bool, String> {
        Confirm::with_theme(&ColorfulTheme::default())
            .with_prompt("Proceed?")
            .default(true)
            .interact()
            .map_err(|err| format!("failed to read confirmation: {err}"))
    }

    fn non_interactive_fallback(&self) -> Result<bool, String> {
        Err(
            "confirmation is required when not running interactively (pass --yes to skip)"
                .to_string(),
        )
    }
}

fn fail(message: impl std::fmt::Display) -> i32 {
    eprintln!("Error: {message}");
    FAILURE_EXIT_CODE
}

/// Entry point for `pigeon job run decrypt-files` (ADR-0028).
pub fn dispatch(
    input_dir: Option<PathBuf>,
    output_dir: Option<PathBuf>,
    encryption_key: Option<String>,
    concurrency: Option<usize>,
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
        yes,
    ))
}

async fn dispatch_async(
    input_dir: Option<PathBuf>,
    output_dir: Option<PathBuf>,
    encryption_key: Option<String>,
    concurrency: Option<usize>,
    yes: bool,
) -> i32 {
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

    let concurrency = match (ConcurrencyInput { flag: concurrency }).resolve() {
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

    match job.run(plan, concurrency).await {
        Ok(summary) => {
            println!(
                "Decrypted {} file(s), {} failed.",
                summary.decrypted, summary.failed
            );
            if summary.failed > 0 {
                FAILURE_EXIT_CODE
            } else {
                0
            }
        }
        Err(err) => fail(err),
    }
}
