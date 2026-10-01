use std::path::PathBuf;

use dialoguer::{Input, MultiSelect, theme::ColorfulTheme};

use crate::commands::FAILURE_EXIT_CODE;
use crate::commands::job::email_sync::wizard::print_manifest_summary;
use crate::commands::job::email_sync::{DEFAULT_MAX_CONNECTIONS_PER_IDENTITY, IdentityContext};
use crate::commands::job::shared_wizard::{ConfirmInput, UploadTargetInput};
use crate::commands::keyring::email::identity::Identity;
use crate::commands::keyring::store::Store;
use crate::core::job::Job;
use crate::core::keyring::credentials;
use crate::core::wizard::WizardInput;

use super::PullJob;

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

pub fn dispatch(
    identities: Option<Vec<String>>,
    local_output: Option<PathBuf>,
    remote_output: Option<String>,
    concurrency: Option<usize>,
    max_connections_per_identity: Option<usize>,
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
        max_connections_per_identity,
        yes,
    ))
}

async fn dispatch_async(
    identities: Option<Vec<String>>,
    local_output: Option<PathBuf>,
    remote_output: Option<String>,
    concurrency: Option<usize>,
    max_connections_per_identity: Option<usize>,
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
            println!(
                "Attachments: {} estimated pre-run, {} actually staged.",
                estimated_pending_attachments, summary.attachments_staged
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

fn fail(message: impl std::fmt::Display) -> i32 {
    eprintln!("Error: {message}");
    FAILURE_EXIT_CODE
}

#[cfg(test)]
mod tests {
    use super::*;

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
