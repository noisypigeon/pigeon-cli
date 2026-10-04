use std::fs;
use std::path::PathBuf;

use dialoguer::{Input, MultiSelect, theme::ColorfulTheme};

use crate::commands::job::email_sync::wizard::print_manifest_summary;
use crate::commands::job::email_sync::{DEFAULT_MAX_CONNECTIONS_PER_IDENTITY, IdentityContext};
use crate::commands::job::report_upload;
use crate::commands::job::shared_wizard::{
    ConfirmInput, UploadConcurrencyInput, UploadTargetInput,
};
use crate::commands::keyring::email::identity::Identity;
use crate::commands::keyring::store::Store;
use crate::commands::{FAILURE_EXIT_CODE, fail};
use crate::core::job::Job;
use crate::core::keyring::credentials;
use crate::core::wizard::WizardInput;

use super::PullJob;
use super::worker;

/// Resolves `--identities` (every alias must exist in the store) or an
/// interactive `MultiSelect`, else errors non-interactively. Own local
/// copy -- `email_sync::wizard`'s `IdentitiesInput` is a private struct,
/// unreachable from this sibling module.
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
            .with_prompt("Select identities to pull")
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

/// Same rough, illustrative time-estimate heuristic as
/// `email_sync::wizard::ConcurrencyInput` -- copied rather than imported
/// (private to that module), and reasonable to reuse as-is since
/// email-pull's IMAP-bound fetch phase has a comparable throughput
/// profile (no Markdown/HTML conversion step to slow it down further, if
/// anything this job is faster per message).
const ASSUMED_SECONDS_PER_MESSAGE: f64 = 0.5;

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
    println!("Rough time estimate (illustrative, not calibrated):");
    for concurrency in candidate_concurrencies(total_pending_messages) {
        let seconds = estimate_seconds(total_pending_messages, concurrency);
        println!(
            "  concurrency {concurrency:>2}: ~{}",
            format_duration(seconds)
        );
    }
}

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

#[allow(clippy::too_many_arguments)]
pub fn dispatch(
    identities: Option<Vec<String>>,
    local_output: Option<PathBuf>,
    remote_output: Option<String>,
    concurrency: Option<usize>,
    upload_concurrency: Option<usize>,
    max_connections_per_identity: Option<usize>,
    upload_only: bool,
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
        identities,
        local_output,
        remote_output,
        concurrency,
        upload_concurrency,
        max_connections_per_identity,
        upload_only,
        report_bucket,
        job_name,
        yes,
    ))
}

#[allow(clippy::too_many_arguments)]
async fn dispatch_async(
    identities: Option<Vec<String>>,
    local_output: Option<PathBuf>,
    remote_output: Option<String>,
    concurrency: Option<usize>,
    upload_concurrency: Option<usize>,
    max_connections_per_identity: Option<usize>,
    upload_only: bool,
    report_bucket: Option<String>,
    job_name: &'static str,
    yes: bool,
) -> i32 {
    // Held for this whole async fn's lifetime, same discipline as
    // `email_sync::wizard::dispatch_async` (ADR-0073).
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
            upload_concurrency,
            report_bucket,
            job_name,
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

    let mut job = PullJob {
        contexts,
        remote: None,
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
    let (transcript, transcript_path) = match report_upload::new_transcript(&local_output) {
        Ok(value) => value,
        Err(err) => return fail(err),
    };

    let (exit_code, report_path) = match job.run(plan, concurrency, upload_concurrency).await {
        Ok(summary) => {
            let message = format!(
                "Pulled {} message(s), {} failed ({} connect, {} examine, {} batch-error, {} missing-file), {} attachment extraction warning(s), {} attachment(s) deduped, {} uploaded, {} unchanged, {} upload failed.",
                summary.synced,
                summary.failed,
                summary.failure_breakdown.connect,
                summary.failure_breakdown.examine,
                summary.failure_breakdown.batch_error,
                summary.failure_breakdown.missing_file,
                summary.attachment_extraction_failed,
                summary.deduped_attachments,
                summary.uploaded,
                summary.unchanged,
                summary.upload_failed
            );
            report_upload::say(&transcript, message);
            report_upload::say(
                &transcript,
                format!(
                    "Attachments: {} estimated pre-run, {} actually staged.",
                    estimated_pending_attachments, summary.attachments_staged
                ),
            );
            let exit_code = if summary.failed > 0 || summary.upload_failed > 0 {
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
            let report_path = report_upload::write_summary_report(&local_output, job_name, &err)
                .unwrap_or_else(|_| local_output.join(format!("{job_name}-report.txt")));
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

/// Whether `ctx`'s identity has a completed local run `--upload-only` can
/// resume uploading from -- same check as `email_sync::wizard`'s version
/// (ADR-0090).
fn identity_has_completed_run(ctx: &IdentityContext) -> bool {
    fs::read_dir(&ctx.output_dir)
        .map(|mut entries| entries.next().is_some())
        .unwrap_or(false)
}

/// `--upload-only` branch of `dispatch_async` (ADR-0090): resumes uploading
/// already-completed local runs for the selected identities, skipping the
/// IMAP connect/fetch/dedup phases -- and the per-identity IMAP
/// credentials they'd otherwise need -- entirely. An identity whose local
/// state isn't ready is skipped with a warning rather than failing the
/// whole command, same as `email_sync::wizard`'s version.
#[allow(clippy::too_many_arguments)]
async fn dispatch_upload_only(
    identities: Option<Vec<String>>,
    local_output: Option<PathBuf>,
    remote_output: Option<String>,
    upload_concurrency: Option<usize>,
    report_bucket: Option<String>,
    job_name: &'static str,
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
            // No IMAP secret lookup here -- this mode never connects
            // (same reasoning as `email_sync::wizard`'s version).
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

    let upload_concurrency = match (UploadConcurrencyInput {
        flag: upload_concurrency,
    })
    .resolve()
    {
        Ok(value) => value,
        Err(err) => return fail(err),
    };
    let (report_bucket_config, report_secret) =
        match report_upload::resolve(report_bucket, keyring_store) {
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
    let (transcript, transcript_path) = match report_upload::new_transcript(&local_output) {
        Ok(value) => value,
        Err(err) => return fail(err),
    };

    let (exit_code, report_path) = match worker::run_upload_only(
        &ready_contexts,
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
            let report_path = report_upload::write_summary_report(&local_output, job_name, &err)
                .unwrap_or_else(|_| local_output.join(format!("{job_name}-report.txt")));
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
    fn identity_has_completed_run_is_true_once_something_was_placed() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("willow-example.com")).unwrap();
        let ctx = identity_ctx(dir.path().to_path_buf());
        assert!(identity_has_completed_run(&ctx));
    }

    #[test]
    fn default_local_output_is_under_temp_dir() {
        assert_eq!(
            default_local_output(),
            std::env::temp_dir().join("pigeon-job")
        );
    }

    #[test]
    fn candidate_concurrencies_caps_at_total_pending() {
        assert_eq!(candidate_concurrencies(3), vec![1, 2]);
    }

    #[test]
    fn candidate_concurrencies_never_empty() {
        assert_eq!(candidate_concurrencies(0), vec![1]);
    }

    #[test]
    fn estimate_seconds_divides_by_concurrency() {
        assert_eq!(estimate_seconds(100, 4), 12.5);
    }

    #[test]
    fn format_duration_formats_hours_minutes_seconds() {
        assert_eq!(format_duration(45.0), "45s");
        assert_eq!(format_duration(125.0), "2m5s");
        assert_eq!(format_duration(3725.0), "1h2m");
    }
}
