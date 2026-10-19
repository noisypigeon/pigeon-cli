//! `pigeon job run email import`'s wizard (CLI-0146 §9).
//!
//! `IdentitiesInput` and `LocalOutputInput` are deliberate duplicates of
//! the sibling email jobs' (CLI-0146 §2): a third consumer is normally this
//! codebase's hoist trigger, but CLI-0147 returns both to a single consumer,
//! so hoisting now would mean un-hoisting next -- the churn CLI-0094 and
//! CLI-0131 each paid once already.

use std::path::PathBuf;

use dialoguer::{Input, MultiSelect, theme::ColorfulTheme};

use crate::commands::job::report_upload;
use crate::commands::job::shared_wizard::{ConfirmInput, load_keyring, prepare_rclone};
use crate::commands::keyring::email::identity::Identity;
use crate::commands::keyring::store::Store;
use crate::commands::{FAILURE_EXIT_CODE, fail, print_table};
use crate::core::job::Job;
use crate::core::keyring::credentials;
use crate::core::rclone::config::RemoteTarget;
use crate::core::wizard::WizardInput;

use super::{DEFAULT_MAX_CONNECTIONS, ImportContext, ImportJob};

/// Deliberately conservative: a run exists to stay welcome on a server for
/// days, so the default pace matters more than the default throughput.
/// 120/min puts a 200,000-message account at roughly 28 hours.
///
/// There is no "unlimited" spelling, which is the whole point -- the
/// absence of a cap is what this job exists to fix (CLI-0146 Consequences).
const DEFAULT_MESSAGES_PER_MINUTE: usize = 120;

/// How many UIDs one `UID FETCH` asks for at a time. Small enough that a
/// failed batch loses little work and a rate-limited server is not handed a
/// thousand-UID request, large enough to amortise the round trip.
const DEFAULT_BATCH_SIZE: usize = 20;

/// Concurrent `rclone copyto` pushes. Deliberately not
/// `shared_wizard::UploadConcurrencyInput`'s 16: that value was tuned
/// (CLI-0091 §3) for a dedicated upload phase racing to finish, whereas
/// this job's pushes interleave with a deliberately throttled fetch, so the
/// push leg never needs to outpace it.
const DEFAULT_PUSH_CONCURRENCY: usize = 4;

pub(crate) struct IdentitiesInput<'a> {
    pub flag: Option<Vec<String>>,
    pub store: &'a Store,
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
            .with_prompt("Select identities to import")
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

/// Where to deliver to: a configured bucket-config alias, optionally
/// composed with a prefix inside it (CLI-0138). An omitted prefix is the
/// bucket root -- unlike `transform`, this job has no `--input-file-type`
/// to derive a default from.
pub(crate) struct DestinationInput<'a> {
    pub flag: Option<String>,
    pub store: &'a Store,
}

impl WizardInput for DestinationInput<'_> {
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
        Err("--destination is required when not running interactively".to_string())
    }
}

fn default_local_output() -> PathBuf {
    std::env::temp_dir().join("pigeon-job")
}

pub(crate) struct LocalOutputInput {
    pub flag: Option<PathBuf>,
}

impl WizardInput for LocalOutputInput {
    type Value = PathBuf;

    fn flag_value(&self) -> Option<Result<PathBuf, String>> {
        self.flag.clone().map(Ok)
    }

    fn prompt(&self) -> Result<PathBuf, String> {
        let default = default_local_output();
        let value = Input::<String>::new()
            .with_prompt("Local directory to stage in-flight messages under")
            .default(default.display().to_string())
            .interact_text()
            .map_err(|err| format!("failed to read local output directory: {err}"))?;
        Ok(PathBuf::from(value))
    }

    fn non_interactive_fallback(&self) -> Result<PathBuf, String> {
        Ok(default_local_output())
    }
}

/// Every numeric input below falls back to its default rather than erroring
/// non-interactively, per `UploadConcurrencyInput`'s CLI-0091 §3 reasoning:
/// unattended operation is this job's primary mode, not an afterthought.
macro_rules! numeric_input {
    ($name:ident, $default:expr, $prompt:literal, $flag:literal) => {
        pub(crate) struct $name {
            pub flag: Option<usize>,
        }

        impl WizardInput for $name {
            type Value = usize;

            fn flag_value(&self) -> Option<Result<usize, String>> {
                self.flag.map(|value| Ok(value.max(1)))
            }

            fn prompt(&self) -> Result<usize, String> {
                let value = Input::<usize>::new()
                    .with_prompt($prompt)
                    .default($default)
                    .interact_text()
                    .map_err(|err| format!(concat!("failed to read ", $flag, ": {}"), err))?;
                Ok(value.max(1))
            }

            fn non_interactive_fallback(&self) -> Result<usize, String> {
                Ok($default)
            }
        }
    };
}

numeric_input!(
    MessagesPerMinuteInput,
    DEFAULT_MESSAGES_PER_MINUTE,
    "Messages per minute",
    "messages per minute"
);
numeric_input!(
    BatchSizeInput,
    DEFAULT_BATCH_SIZE,
    "UIDs per IMAP fetch",
    "batch size"
);
numeric_input!(
    MaxConnectionsInput,
    DEFAULT_MAX_CONNECTIONS,
    "Max IMAP connections per identity",
    "max connections"
);
numeric_input!(
    PushConcurrencyInput,
    DEFAULT_PUSH_CONCURRENCY,
    "Concurrent pushes",
    "push concurrency"
);

/// The pre-run table. Counts only -- no `BYTES` or `ATTACHMENTS` columns,
/// since producing either costs a second full-corpus IMAP traversal
/// (CLI-0146 §7).
fn print_summary(summaries: &[super::IdentitySummary]) {
    let rows: Vec<Vec<String>> = summaries
        .iter()
        .map(|summary| {
            vec![
                summary.alias.clone(),
                summary.mailboxes.to_string(),
                summary.pending_messages.to_string(),
            ]
        })
        .collect();
    print_table(&["IDENTITY", "MAILBOXES", "PENDING"], &rows);
}

/// Rough wall-clock estimate from the pace alone. Honest about being a
/// floor: the pacer sets a ceiling on throughput, so a real run is at least
/// this long and never shorter.
fn estimate_duration(total_messages: usize, messages_per_minute: usize) -> String {
    let minutes = total_messages as f64 / messages_per_minute.max(1) as f64;
    if minutes < 60.0 {
        format!("{minutes:.0}m")
    } else if minutes < 60.0 * 48.0 {
        format!("{:.1}h", minutes / 60.0)
    } else {
        format!("{:.1}d", minutes / 60.0 / 24.0)
    }
}

#[allow(clippy::too_many_arguments)]
pub fn dispatch(
    identities: Option<Vec<String>>,
    destination: Option<String>,
    local_output: Option<PathBuf>,
    messages_per_minute: Option<usize>,
    batch_size: Option<usize>,
    max_connections: Option<usize>,
    push_concurrency: Option<usize>,
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
        identities,
        destination,
        local_output,
        messages_per_minute,
        batch_size,
        max_connections,
        push_concurrency,
        report_bucket,
        job_name,
        non_interactive,
    ))
}

#[allow(clippy::too_many_arguments)]
async fn dispatch_async(
    identities: Option<Vec<String>>,
    destination: Option<String>,
    local_output: Option<PathBuf>,
    messages_per_minute: Option<usize>,
    batch_size: Option<usize>,
    max_connections: Option<usize>,
    push_concurrency: Option<usize>,
    report_bucket: Option<String>,
    job_name: &'static str,
    non_interactive: bool,
) -> i32 {
    // Held for this whole async fn's lifetime, so every early return drops
    // and aborts it (CLI-0073).
    let _sampler =
        crate::observability::resources::ResourceSampler::spawn(std::time::Duration::from_secs(5));

    let keyring_store = match load_keyring() {
        Ok(store) => store,
        Err(err) => return fail(err),
    };

    // The first email job to need either half of this: both siblings are
    // `minio`-only and need neither the binary nor the generated config
    // (CLI-0146 §9).
    if let Err(err) = prepare_rclone(&keyring_store).await {
        return fail(err);
    }

    let selected_identities = match (IdentitiesInput {
        flag: identities,
        store: &keyring_store,
    })
    .resolve(non_interactive)
    {
        Ok(identities) => identities,
        Err(err) => return fail(err),
    };

    let destination = match (DestinationInput {
        flag: destination,
        store: &keyring_store,
    })
    .resolve(non_interactive)
    {
        Ok(target) => target,
        Err(err) => return fail(err),
    };

    let local_output = match (LocalOutputInput { flag: local_output }).resolve(non_interactive) {
        Ok(path) => path,
        Err(err) => return fail(err),
    };

    let messages_per_minute = match (MessagesPerMinuteInput {
        flag: messages_per_minute,
    })
    .resolve(non_interactive)
    {
        Ok(value) => value,
        Err(err) => return fail(err),
    };
    let batch_size = match (BatchSizeInput { flag: batch_size }).resolve(non_interactive) {
        Ok(value) => value,
        Err(err) => return fail(err),
    };
    let max_connections = match (MaxConnectionsInput {
        flag: max_connections,
    })
    .resolve(non_interactive)
    {
        Ok(value) => value,
        Err(err) => return fail(err),
    };
    let push_concurrency = match (PushConcurrencyInput {
        flag: push_concurrency,
    })
    .resolve(non_interactive)
    {
        Ok(value) => value,
        Err(err) => return fail(err),
    };

    let report_bucket_config =
        match report_upload::resolve(report_bucket, &keyring_store, non_interactive) {
            Ok(value) => value,
            Err(err) => return fail(err),
        };

    if let Err(err) = std::fs::create_dir_all(&local_output) {
        return fail(format!(
            "failed to create {}: {err}",
            local_output.display()
        ));
    }

    let contexts = selected_identities
        .iter()
        .map(|identity| {
            let secret = credentials::resolve_secret(&identity.alias, &identity.secret);
            let identity_root = local_output.join(&identity.alias);
            ImportContext {
                identity: identity.clone(),
                secret,
                staging_dir: identity_root.join(".staging"),
                scratch_dir: identity_root.join("scratch"),
            }
        })
        .collect();

    let job = ImportJob {
        contexts,
        destination,
        messages_per_minute,
        batch_size,
        max_connections,
        log_dir: local_output.clone(),
    };

    let plan = match job.gather().await {
        Ok(plan) => plan,
        Err(err) => return fail(err),
    };

    print_summary(&plan.summaries);
    let total_pending: usize = plan
        .summaries
        .iter()
        .map(|summary| summary.pending_messages)
        .sum();
    if total_pending == 0 {
        println!("Everything is already delivered.");
        return 0;
    }
    println!(
        "Destination: {}\nPace: {messages_per_minute} message(s)/minute -- at least {} for {total_pending} message(s).",
        job.destination.address(),
        estimate_duration(total_pending, messages_per_minute),
    );

    match (ConfirmInput { non_interactive }).resolve(non_interactive) {
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

    // `concurrency` is unused by this job (CLI-0146 §1); `push_concurrency`
    // travels in the `Job` trait's `upload_concurrency` slot.
    let (exit_code, report_path) = match job.run(plan, 0, push_concurrency).await {
        Ok(summary) => {
            report_upload::say(
                &transcript,
                format!(
                    "Delivered {} message(s) and {} attachment(s) ({} object(s) pushed), {} failed ({} connect, {} examine, {} fetch, {} missing-file, {} push).",
                    summary.delivered,
                    summary.attachments_pushed,
                    summary.objects_pushed,
                    summary.failed,
                    summary.failure_breakdown.connect,
                    summary.failure_breakdown.examine,
                    summary.failure_breakdown.fetch,
                    summary.failure_breakdown.missing_file,
                    summary.failure_breakdown.push,
                ),
            );
            if summary.attachment_extraction_failed > 0 {
                report_upload::say(
                    &transcript,
                    format!(
                        "{} message(s) could not be parsed; their raw .eml was still delivered.",
                        summary.attachment_extraction_failed
                    ),
                );
            }
            if summary.gate_give_ups > 0 {
                report_upload::say(
                    &transcript,
                    format!(
                        "{} failure(s) happened after an endpoint stayed unreachable past the recovery budget -- a rerun will pick them up.",
                        summary.gate_give_ups
                    ),
                );
            }
            let exit_code = if summary.failed > 0 {
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
            report_upload::say_error(&transcript, &err);
            let report_path = report_upload::write_summary_report(&local_output, job_name, &err)
                .unwrap_or_else(|_| local_output.join(format!("{job_name}-report.txt")));
            (fail(err), report_path)
        }
    };

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
    fn default_local_output_sits_under_the_temp_dir() {
        assert!(default_local_output().starts_with(std::env::temp_dir()));
    }

    #[test]
    fn identities_input_requires_a_flag_when_not_interactive() {
        let store = Store::default();
        let input = IdentitiesInput {
            flag: None,
            store: &store,
        };
        assert_eq!(
            input.non_interactive_fallback().unwrap_err(),
            "--identities is required when not running interactively"
        );
    }

    #[test]
    fn identities_input_rejects_an_unknown_alias() {
        let store = Store::default();
        let input = IdentitiesInput {
            flag: Some(vec!["nope".to_string()]),
            store: &store,
        };
        // Matched rather than `assert_eq!`d: `Identity` has no `PartialEq`,
        // so the `Ok` half of this `Result` is not comparable.
        match input.flag_value() {
            Some(Err(err)) => assert_eq!(err, "no identity with alias 'nope'"),
            _ => panic!("an unknown alias must not resolve"),
        }
    }

    #[test]
    fn destination_input_requires_a_flag_when_not_interactive() {
        let store = Store::default();
        let input = DestinationInput {
            flag: None,
            store: &store,
        };
        assert!(input.non_interactive_fallback().is_err());
    }

    #[test]
    fn destination_input_rejects_an_unknown_alias() {
        let store = Store::default();
        let input = DestinationInput {
            flag: Some("nope:eml/".to_string()),
            store: &store,
        };
        assert_eq!(
            input.flag_value(),
            Some(Err("no bucket-config named 'nope'".to_string()))
        );
    }

    #[test]
    fn local_output_input_falls_back_to_the_default_when_not_interactive() {
        let input = LocalOutputInput { flag: None };
        assert_eq!(input.non_interactive_fallback(), Ok(default_local_output()));
    }

    #[test]
    fn every_numeric_input_falls_back_to_its_default_when_not_interactive() {
        assert_eq!(
            (MessagesPerMinuteInput { flag: None }).non_interactive_fallback(),
            Ok(DEFAULT_MESSAGES_PER_MINUTE)
        );
        assert_eq!(
            (BatchSizeInput { flag: None }).non_interactive_fallback(),
            Ok(DEFAULT_BATCH_SIZE)
        );
        assert_eq!(
            (MaxConnectionsInput { flag: None }).non_interactive_fallback(),
            Ok(DEFAULT_MAX_CONNECTIONS)
        );
        assert_eq!(
            (PushConcurrencyInput { flag: None }).non_interactive_fallback(),
            Ok(DEFAULT_PUSH_CONCURRENCY)
        );
    }

    #[test]
    fn every_numeric_input_flag_value_overrides_its_default() {
        assert_eq!(
            (MessagesPerMinuteInput { flag: Some(600) }).flag_value(),
            Some(Ok(600))
        );
        assert_eq!(
            (BatchSizeInput { flag: Some(50) }).flag_value(),
            Some(Ok(50))
        );
        assert_eq!(
            (MaxConnectionsInput { flag: Some(4) }).flag_value(),
            Some(Ok(4))
        );
        assert_eq!(
            (PushConcurrencyInput { flag: Some(8) }).flag_value(),
            Some(Ok(8))
        );
    }

    /// A zero would otherwise reach `Pacer::new` or `Semaphore::new`.
    #[test]
    fn numeric_inputs_clamp_a_zero_flag_to_one() {
        assert_eq!(
            (MessagesPerMinuteInput { flag: Some(0) }).flag_value(),
            Some(Ok(1))
        );
        assert_eq!((BatchSizeInput { flag: Some(0) }).flag_value(), Some(Ok(1)));
        assert_eq!(
            (PushConcurrencyInput { flag: Some(0) }).flag_value(),
            Some(Ok(1))
        );
    }

    #[test]
    fn estimate_duration_scales_from_minutes_to_hours_to_days() {
        assert_eq!(estimate_duration(120, 120), "1m");
        assert_eq!(estimate_duration(7_200, 120), "1.0h");
        // The motivating case: a 200k-message account at the default pace.
        assert_eq!(estimate_duration(200_000, 120), "27.8h");
        // Past two days it switches units rather than printing "100.0h".
        assert_eq!(estimate_duration(1_000_000, 120), "5.8d");
    }

    #[test]
    fn estimate_duration_survives_a_zero_rate() {
        // `resolve` clamps to 1, but the formatter must not divide by zero
        // even if a future caller does not.
        assert!(!estimate_duration(10, 0).is_empty());
    }

    /// The default pace must put the motivating workload in the
    /// over-a-day range this job is designed for, not the minutes range --
    /// a faster default would quietly defeat the point of having a pacer.
    #[test]
    fn the_default_pace_is_deliberately_conservative() {
        assert_eq!(DEFAULT_MESSAGES_PER_MINUTE, 120);

        let minutes = 200_000.0 / DEFAULT_MESSAGES_PER_MINUTE as f64;
        assert!(
            minutes > 24.0 * 60.0,
            "200k messages must take over a day at the default pace, got {minutes}m"
        );
    }
}
