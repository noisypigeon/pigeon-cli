use std::fs;
use std::path::PathBuf;

use dialoguer::{Input, MultiSelect, theme::ColorfulTheme};

use crate::commands::FAILURE_EXIT_CODE;
use crate::commands::job::shared_wizard::{
    ConfirmInput, EncryptionKeyInput, UploadConcurrencyInput, UploadTargetInput,
};
use crate::commands::keyring::email::identity::Identity;
use crate::commands::keyring::store::Store;
use crate::core::crypto::Aes256GcmSivEncryptor;
use crate::core::job::Job;
use crate::core::keyring::credentials;
use crate::core::wizard::WizardInput;

use super::worker;
use super::{
    DEFAULT_MAX_CONNECTIONS_PER_IDENTITY, EmailSyncJob, IdentityContext, IdentityManifestSummary,
};

/// Resolves which identities to run against: `--identities` if given (every
/// alias must already exist), an interactive `MultiSelect` if omitted and
/// stdin is a terminal, or a hard error otherwise.
///
/// Per ADR-0021 §8's narrow `--yes` semantics: `--yes` only skips the final
/// proceed confirmation. A missing `--identities` outside a TTY is always
/// an error, regardless of `--yes` -- silently defaulting to "every
/// configured identity" would be a much worse failure mode for a scripted/
/// cron invocation than a fast, explicit error naming the missing flag.
struct IdentitiesInput<'a> {
    flag: Option<Vec<String>>,
    store: &'a Store,
}

impl WizardInput for IdentitiesInput<'_> {
    type Value = Vec<Identity>;

    fn flag_value(&self) -> Option<Result<Vec<Identity>, String>> {
        self.flag.as_ref().map(|aliases| {
            aliases
                .iter()
                .map(|alias| {
                    self.store
                        .email_identities()
                        .find(|identity| &identity.alias == alias)
                        .cloned()
                        .ok_or_else(|| format!("no identity with alias '{alias}'"))
                })
                .collect()
        })
    }

    fn prompt(&self) -> Result<Vec<Identity>, String> {
        let all: Vec<&Identity> = self.store.email_identities().collect();
        if all.is_empty() {
            return Err(
                "no identities configured; run 'pigeon keyring add email' first".to_string(),
            );
        }
        let labels: Vec<String> = all
            .iter()
            .map(|identity| {
                format!(
                    "{} ({}, {})",
                    identity.alias, identity.email, identity.provider
                )
            })
            .collect();
        let selected = MultiSelect::with_theme(&ColorfulTheme::default())
            .with_prompt("Select identities to sync")
            .items(&labels)
            .interact()
            .map_err(|err| format!("failed to read identity selection: {err}"))?;
        if selected.is_empty() {
            return Err("at least one identity must be selected".to_string());
        }
        Ok(selected
            .into_iter()
            .map(|index| all[index].clone())
            .collect())
    }

    fn non_interactive_fallback(&self) -> Result<Vec<Identity>, String> {
        Err("--identities is required when not running interactively".to_string())
    }
}

/// The shared local-output root used when `--local-output` is omitted and
/// there's no interactive prompt to fall back to a chosen value (or the
/// prompt itself is seeded with this as its editable default).
fn default_local_output() -> PathBuf {
    std::env::temp_dir().join("pigeon-job")
}

/// Resolves the shared local-output root (ADR-0021 §5 amendment):
/// `--local-output` if given; an editable `Input` prompt (default
/// `$TMPDIR/pigeon-job`) on a TTY if omitted; that same default silently,
/// no prompt, if omitted and non-interactive -- unlike `IdentitiesInput`/
/// `ConcurrencyInput`, an omitted value here is never an error, since it
/// already had a safe default before this prompt existed (ADR-0021 §8).
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

/// Prints a per-identity manifest summary table (ADR-0021 §5). `ATTACHMENTS`
/// is a `BODYSTRUCTURE`-derived estimate, not authoritative (ADR-0032).
pub(crate) fn print_manifest_summary(summaries: &[IdentityManifestSummary]) {
    let rows: Vec<Vec<String>> = summaries
        .iter()
        .map(|summary| {
            vec![
                summary.alias.clone(),
                summary.mailboxes.to_string(),
                summary.pending_messages.to_string(),
                summary.pending_attachments.to_string(),
                format_bytes(summary.pending_bytes),
            ]
        })
        .collect();
    crate::commands::print_table(
        &["IDENTITY", "MAILBOXES", "PENDING", "ATTACHMENTS", "SIZE"],
        &rows,
    );
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

/// Illustrative, not calibrated -- ADR-0021 §9 is explicit that no
/// historical throughput data exists anywhere in this codebase, so this is
/// a rough guide, never presented as a guarantee.
const ASSUMED_SECONDS_PER_MESSAGE: f64 = 0.5;

/// A handful of concurrency levels worth showing an estimate for, capped at
/// `total_pending` (no point suggesting a concurrency higher than the
/// number of messages there are to fetch).
fn candidate_concurrencies(total_pending: usize) -> Vec<usize> {
    let candidates = [1, 2, 4, 8, 16];
    let capped: Vec<usize> = candidates
        .into_iter()
        .filter(|candidate| *candidate <= total_pending.max(1))
        .collect();
    if capped.is_empty() { vec![1] } else { capped }
}

fn estimate_seconds(pending_messages: usize, concurrency: usize) -> f64 {
    (pending_messages as f64 * ASSUMED_SECONDS_PER_MESSAGE) / concurrency.max(1) as f64
}

fn format_duration(seconds: f64) -> String {
    let total = seconds.round() as u64;
    if total < 60 {
        format!("{total}s")
    } else if total < 3600 {
        format!("{}m{}s", total / 60, total % 60)
    } else {
        format!("{}h{}m", total / 3600, (total % 3600) / 60)
    }
}

fn print_concurrency_estimate(total_pending_messages: usize) {
    println!("Rough time estimate (illustrative, not calibrated -- ADR-0021 §9):");
    for concurrency in candidate_concurrencies(total_pending_messages) {
        let seconds = estimate_seconds(total_pending_messages, concurrency);
        println!(
            "  concurrency {concurrency:>2}: ~{}",
            format_duration(seconds)
        );
    }
}

/// Resolves the concurrency to run at: `--concurrency` if given, an
/// interactive prompt (with the estimate table above) if omitted and stdin
/// is a terminal, or a hard error otherwise -- same narrow-`--yes` rule as
/// `IdentitiesInput`.
struct ConcurrencyInput {
    flag: Option<usize>,
    total_pending: usize,
}

impl WizardInput for ConcurrencyInput {
    type Value = usize;

    fn flag_value(&self) -> Option<Result<usize, String>> {
        self.flag.map(|value| Ok(value.max(1)))
    }

    fn prompt(&self) -> Result<usize, String> {
        print_concurrency_estimate(self.total_pending);
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

/// Entry point for `pigeon job run email-sync` -- the wizard flow of
/// ADR-0021 §5: resolve identities → pull/load each one's manifest → show
/// the summary and resolve concurrency (with a time estimate) → confirm →
/// run the four-phase pipeline.
#[allow(clippy::too_many_arguments)]
pub fn dispatch(
    identities: Option<Vec<String>>,
    local_output: Option<PathBuf>,
    remote_output: Option<String>,
    encryption_key: Option<String>,
    concurrency: Option<usize>,
    upload_concurrency: Option<usize>,
    max_connections_per_identity: Option<usize>,
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
        identities,
        local_output,
        remote_output,
        encryption_key,
        concurrency,
        upload_concurrency,
        max_connections_per_identity,
        upload_only,
        yes,
    ))
}

#[allow(clippy::too_many_arguments)]
async fn dispatch_async(
    identities: Option<Vec<String>>,
    local_output: Option<PathBuf>,
    remote_output: Option<String>,
    encryption_key: Option<String>,
    concurrency: Option<usize>,
    upload_concurrency: Option<usize>,
    max_connections_per_identity: Option<usize>,
    upload_only: bool,
    yes: bool,
) -> i32 {
    // Held for this whole async fn's lifetime -- every early `return fail(...)`
    // below drops it, aborting the sampling task automatically (ADR-0073).
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
            identities,
            local_output,
            remote_output,
            encryption_key,
            upload_concurrency,
            yes,
            &keyring_store,
        )
        .await;
    }

    let selected_identities = match (IdentitiesInput {
        flag: identities,
        store: &keyring_store,
    })
    .resolve()
    {
        Ok(identities) => identities,
        Err(err) => return fail(err),
    };

    let local_output = match (LocalOutputInput { flag: local_output }).resolve() {
        Ok(path) => path,
        Err(err) => return fail(err),
    };

    let mut contexts = Vec::new();
    for identity in &selected_identities {
        let secret = match credentials::get_secret(&identity.alias) {
            Ok(secret) => secret,
            Err(err) => return fail(err),
        };
        let identity_root = local_output.join(&identity.alias);
        contexts.push(IdentityContext {
            identity: identity.clone(),
            secret,
            staging_dir: identity_root.join("staging"),
            output_dir: identity_root.join("result"),
        });
    }

    let mut job = EmailSyncJob {
        contexts,
        remote: None,
        encryptor: None,
        max_connections_per_identity: max_connections_per_identity
            .unwrap_or(DEFAULT_MAX_CONNECTIONS_PER_IDENTITY),
    };
    let plan = match job.gather().await {
        Ok(plan) => plan,
        Err(err) => return fail(err),
    };

    print_manifest_summary(&plan.manifest_summaries);
    let total_pending: usize = plan
        .manifest_summaries
        .iter()
        .map(|summary| summary.pending_messages)
        .sum();
    // Captured before `plan` moves into `job.run` below, so the final
    // summary line can show the pre-run manifest estimate next to the real,
    // post-run `attachments_staged` count (ADR-0033 #42).
    let estimated_pending_attachments: usize = plan
        .manifest_summaries
        .iter()
        .map(|summary| summary.pending_attachments)
        .sum();
    if total_pending == 0 {
        println!("Everything is already up to date.");
        return 0;
    }

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

    // Resolved here, before the concurrency/proceed prompts and any
    // fetch/transform work -- the target bucket-config's own default key (if
    // any) is used unless overridden by flag or interactively (ADR-0027).
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

    let concurrency = match (ConcurrencyInput {
        flag: concurrency,
        total_pending,
    })
    .resolve()
    {
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
                "Synced {} message(s), {} failed ({} connect, {} examine, {} batch-error, {} verification, {} parse-skipped, {} missing-file), {} message(s) merged, {} attachment(s) deduped, {} uploaded, {} unchanged, {} upload failed.",
                summary.synced,
                summary.failed,
                summary.failure_breakdown.connect,
                summary.failure_breakdown.examine,
                summary.failure_breakdown.batch_error,
                summary.failure_breakdown.verification,
                summary.failure_breakdown.parse_skipped,
                summary.failure_breakdown.missing_file,
                summary.merged_messages,
                summary.deduped_attachments,
                summary.uploaded,
                summary.unchanged,
                summary.upload_failed
            );
            println!(
                "Attachments: {} estimated pre-run, {} actually staged.",
                estimated_pending_attachments, summary.attachments_staged
            );
            // A worker absorbing a connect/fetch failure into `failed`
            // (ADR-0021 §6 addendum) lets the run complete and checkpoint
            // everything that succeeded, but that must still be visible to
            // a script checking the exit code -- otherwise a partially
            // failed run would silently report success.
            if summary.failed > 0 || summary.upload_failed > 0 {
                FAILURE_EXIT_CODE
            } else {
                0
            }
        }
        Err(err) => fail(err),
    }
}

fn fail(message: impl std::fmt::Display) -> i32 {
    eprintln!("Error: {message}");
    FAILURE_EXIT_CODE
}

/// Whether `ctx`'s identity has a completed local run `--upload-only` can
/// resume uploading from: its own `output_dir` must exist and hold at
/// least one entry (the sanitized-email-named subdirectory a prior run
/// placed its result under). Per-identity, unlike `deduplicate`'s single-tree
/// check, since each identity has its own isolated `staging`/`result`
/// subtree (ADR-0090).
fn identity_has_completed_run(ctx: &IdentityContext) -> bool {
    fs::read_dir(&ctx.output_dir)
        .map(|mut entries| entries.next().is_some())
        .unwrap_or(false)
}

/// `--upload-only` branch of `dispatch_async` (ADR-0090): resumes uploading
/// already-completed local runs for the selected identities, skipping the
/// IMAP connect/fetch/transform/dedup phases -- and the per-identity IMAP
/// credentials they'd otherwise need -- entirely. Unlike `deduplicate`'s
/// single-tree version, an identity whose local state isn't ready is
/// skipped with a warning rather than failing the whole command, since the
/// other selected identities may still have something to upload.
async fn dispatch_upload_only(
    identities: Option<Vec<String>>,
    local_output: Option<PathBuf>,
    remote_output: Option<String>,
    encryption_key: Option<String>,
    upload_concurrency: Option<usize>,
    yes: bool,
    keyring_store: &Store,
) -> i32 {
    let selected_identities = match (IdentitiesInput {
        flag: identities,
        store: keyring_store,
    })
    .resolve()
    {
        Ok(identities) => identities,
        Err(err) => return fail(err),
    };
    let local_output = match (LocalOutputInput { flag: local_output }).resolve() {
        Ok(path) => path,
        Err(err) => return fail(err),
    };

    let mut ready_contexts = Vec::new();
    for identity in &selected_identities {
        let identity_root = local_output.join(&identity.alias);
        let ctx = IdentityContext {
            identity: identity.clone(),
            // No IMAP secret lookup here -- this mode never connects, so
            // `IdentityContext.secret` (an IMAP credential) is never read
            // on this path; a placeholder keeps the struct's one required
            // field satisfied without touching the OS keychain (ADR-0090).
            secret: String::new(),
            staging_dir: identity_root.join("staging"),
            output_dir: identity_root.join("result"),
        };
        if identity_has_completed_run(&ctx) {
            ready_contexts.push(ctx);
        } else {
            println!(
                "Warning: skipping '{}': no completed local run found under {}",
                ctx.identity.alias,
                ctx.output_dir.display()
            );
        }
    }
    if ready_contexts.is_empty() {
        return fail(
            "no completed local run found for any selected identity; run without --upload-only first",
        );
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
        &ready_contexts,
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
    use crate::commands::keyring::email::provider::Provider;

    fn identity_ctx(output_dir: PathBuf) -> IdentityContext {
        IdentityContext {
            identity: Identity {
                alias: "work".to_string(),
                email: "willow@example.com".to_string(),
                provider: Provider::Gmail,
                host: "imap.gmail.com".to_string(),
                port: 993,
                max_imap_connections: None,
            },
            secret: "unused".to_string(),
            staging_dir: PathBuf::from("/unused/staging"),
            output_dir,
        }
    }

    #[test]
    fn identity_has_completed_run_is_false_for_a_missing_output_dir() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = identity_ctx(dir.path().join("does-not-exist"));
        assert!(!identity_has_completed_run(&ctx));
    }

    #[test]
    fn identity_has_completed_run_is_false_for_an_empty_output_dir() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = identity_ctx(dir.path().to_path_buf());
        assert!(!identity_has_completed_run(&ctx));
    }

    #[test]
    fn identity_has_completed_run_is_true_once_something_was_placed() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("willow-example.com")).unwrap();
        let ctx = identity_ctx(dir.path().to_path_buf());
        assert!(identity_has_completed_run(&ctx));
    }

    #[test]
    fn default_local_output_is_under_the_os_temp_dir() {
        let path = default_local_output();
        assert!(path.starts_with(std::env::temp_dir()));
        assert_eq!(path.file_name().unwrap(), "pigeon-job");
    }

    #[test]
    fn resolve_local_output_returns_given_path_unchanged() {
        let given = PathBuf::from("/explicit/path");
        assert_eq!(
            (LocalOutputInput {
                flag: Some(given.clone())
            })
            .resolve()
            .unwrap(),
            given
        );
    }

    #[test]
    fn resolve_remote_output_returns_given_alias_unchanged() {
        let store = Store::default();
        assert_eq!(
            (UploadTargetInput {
                flag: Some("backup".to_string()),
                store: &store,
            })
            .resolve()
            .unwrap(),
            Some("backup".to_string())
        );
    }

    #[test]
    fn candidate_concurrencies_caps_at_total_pending() {
        assert_eq!(candidate_concurrencies(3), vec![1, 2]);
    }

    #[test]
    fn candidate_concurrencies_zero_pending_still_offers_one() {
        assert_eq!(candidate_concurrencies(0), vec![1]);
    }

    #[test]
    fn candidate_concurrencies_large_input_offers_every_level() {
        assert_eq!(candidate_concurrencies(1000), vec![1, 2, 4, 8, 16]);
    }

    #[test]
    fn estimate_seconds_scales_inversely_with_concurrency() {
        let at_one = estimate_seconds(100, 1);
        let at_four = estimate_seconds(100, 4);
        assert_eq!(at_four, at_one / 4.0);
    }

    #[test]
    fn estimate_seconds_zero_concurrency_does_not_divide_by_zero() {
        assert!(estimate_seconds(100, 0).is_finite());
    }

    #[test]
    fn format_bytes_stays_in_bytes_under_a_kib() {
        assert_eq!(format_bytes(512), "512 B");
    }

    #[test]
    fn format_bytes_uses_larger_units_for_larger_sizes() {
        assert_eq!(format_bytes(1024), "1.0 KB");
        assert_eq!(format_bytes(1024 * 1024 * 3), "3.0 MB");
    }

    #[test]
    fn format_duration_under_a_minute_is_seconds_only() {
        assert_eq!(format_duration(45.0), "45s");
    }

    #[test]
    fn format_duration_formats_minutes_and_hours() {
        assert_eq!(format_duration(125.0), "2m5s");
        assert_eq!(format_duration(3725.0), "1h2m");
    }
}
