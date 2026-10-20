//! `pigeon job run email scan`'s wizard (CLI-0164 §9).
//!
//! `IdentitiesInput`, `LocalOutputInput`, and `DestinationInput` are reused
//! from `email_import::wizard` rather than copied: two consumers, and the
//! prompts are identical. Hoisting into `shared_wizard` waits for a third,
//! per this codebase's standing rule (CLI-0082 §0).
//!
//! There is no "proceed?" confirmation. The scan transfers no message and
//! changes nothing on the server; its only writes are to `--ledger-bucket`
//! (the status file, and a ledger seed on first sight of an identity).

use std::path::PathBuf;

use indicatif::MultiProgress;

use crate::commands::job::email_import::ImportContext;
use crate::commands::job::email_import::wizard::{IdentitiesInput, LocalOutputInput};
use crate::commands::job::report_upload;
use crate::commands::job::shared_wizard::{LedgerBucketInput, load_keyring, prepare_rclone};
use crate::commands::{FAILURE_EXIT_CODE, fail, print_table};
use crate::core::keyring::credentials;
use crate::core::rclone::config::RemoteTarget;
use crate::core::wizard::WizardInput;

use super::status;
use super::{IdentityScan, scan_identity};

pub fn dispatch(
    identities: Option<Vec<String>>,
    ledger_bucket: Option<String>,
    destination: Option<String>,
    local_output: Option<PathBuf>,
    debug_log_bucket: Option<String>,
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
        ledger_bucket,
        destination,
        local_output,
        debug_log_bucket,
        job_name,
        non_interactive,
    ))
}

async fn dispatch_async(
    identities: Option<Vec<String>>,
    ledger_bucket: Option<String>,
    destination: Option<String>,
    local_output: Option<PathBuf>,
    debug_log_bucket: Option<String>,
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

    let ledger_target = match (LedgerBucketInput {
        flag: ledger_bucket,
        store: &keyring_store,
    })
    .resolve(non_interactive)
    {
        Ok(target) => target,
        Err(err) => return fail(err),
    };

    // Only consulted to seed a missing ledger, so never prompted for: an
    // identity that needs it and lacks it fails with a message naming the
    // flag (`ledger::load_or_seed`). Parsed up front so a typo'd alias
    // fails before any IMAP work.
    let destination: Option<RemoteTarget> = match destination
        .as_deref()
        .map(|value| RemoteTarget::parse(value, &keyring_store))
        .transpose()
    {
        Ok(target) => target,
        Err(err) => return fail(err),
    };

    let local_output = match (LocalOutputInput { flag: local_output }).resolve(non_interactive) {
        Ok(path) => path,
        Err(err) => return fail(err),
    };

    let debug_log_bucket_config =
        match report_upload::resolve(debug_log_bucket, &keyring_store, non_interactive) {
            Ok(value) => value,
            Err(err) => return fail(err),
        };

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

    report_upload::say(
        &transcript,
        format!(
            "Scanning {} identit{} against {}.",
            selected_identities.len(),
            if selected_identities.len() == 1 {
                "y"
            } else {
                "ies"
            },
            ledger_target.address()
        ),
    );

    let multi_progress = MultiProgress::new();
    let mut scans: Vec<IdentityScan> = Vec::new();
    let mut failures: Vec<(String, String)> = Vec::new();
    for identity in &selected_identities {
        let identity_root = local_output.join(&identity.alias);
        let ctx = ImportContext {
            identity: identity.clone(),
            secret: credentials::resolve_secret(&identity.alias, &identity.secret),
            staging_dir: identity_root.join(".staging"),
            scratch_dir: identity_root.join("scratch"),
            ledger: std::sync::OnceLock::new(),
        };
        // One identity's failure does not abandon the rest: a scheduled
        // scan should report every account it could reach.
        match scan_identity(
            &ctx,
            &ledger_target,
            destination.as_ref(),
            &local_output.join("state-logs"),
            &multi_progress,
        )
        .await
        {
            Ok(scan) => {
                if scan.seeded {
                    report_upload::say(
                        &transcript,
                        format!(
                            "{}: no ledger existed; seeded it from the destination.",
                            scan.alias
                        ),
                    );
                }
                if scan.unparsed_bodystructures > 0 {
                    report_upload::say(
                        &transcript,
                        format!(
                            "{}: {} new message(s) had an unparseable BODYSTRUCTURE and were counted as having no attachments.",
                            scan.alias, scan.unparsed_bodystructures
                        ),
                    );
                }
                scans.push(scan);
            }
            Err(err) => {
                tracing::error!(identity = %identity.alias, error = %err, "email-scan: identity failed");
                report_upload::say_error(&transcript, format!("{}: {err}", identity.alias));
                failures.push((identity.alias.clone(), err));
            }
        }
    }

    print_summary(&scans);
    let total_new: usize = scans
        .iter()
        .filter_map(IdentityScan::total)
        .map(|total| total.new_messages)
        .sum();
    report_upload::say(
        &transcript,
        format!(
            "{total_new} new message(s) across {} identit{}; {} failed.",
            scans.len(),
            if scans.len() == 1 { "y" } else { "ies" },
            failures.len()
        ),
    );
    tracing::info!(
        pigeon_job = super::JOB,
        identities = scans.len(),
        failed = failures.len(),
        new_messages = total_new,
        "email-scan: run complete"
    );

    let exit_code = if failures.is_empty() {
        0
    } else {
        FAILURE_EXIT_CODE
    };
    let report_path = local_output.join(format!("{job_name}-report.txt"));
    if let Err(err) = std::fs::write(&report_path, render_report(&scans, &failures)) {
        tracing::warn!(error = %err, "failed to write report");
    }

    report_upload::log_run_outcome(exit_code);
    report_upload::upload_run_artifacts(
        &debug_log_bucket_config,
        &run_prefix,
        &run_id,
        &report_path,
        &transcript_path,
    )
    .await;

    exit_code
}

fn print_summary(scans: &[IdentityScan]) {
    let rows: Vec<Vec<String>> = scans
        .iter()
        .filter_map(|scan| scan.total().map(|total| (scan, total)))
        .map(|(scan, total)| {
            vec![
                scan.alias.clone(),
                (scan.rows.len() - 1).to_string(),
                total.server_messages.to_string(),
                total.ingested.to_string(),
                total.new_messages.to_string(),
                total
                    .new_attachments
                    .map_or_else(|| "unknown".to_string(), |n| n.to_string()),
            ]
        })
        .collect();
    print_table(
        &[
            "IDENTITY",
            "MAILBOXES",
            "SERVER",
            "INGESTED",
            "NEW",
            "NEW ATTACHMENTS",
        ],
        &rows,
    );
}

/// `email-scan-report.txt`: every identity's status rows with an
/// `identity` column prepended, then one line per failed identity.
fn render_report(scans: &[IdentityScan], failures: &[(String, String)]) -> String {
    let mut out = String::from("identity\t");
    out.push_str(status::serialize(&[]).trim_start_matches("# ").trim_end());
    out.push('\n');
    for scan in scans {
        for row in &scan.rows {
            out.push_str(&format!("{}\t{}\n", scan.alias, status::format_row(row)));
        }
    }
    for (alias, err) in failures {
        out.push_str(&format!("# failed\t{alias}\t{err}\n"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::status::MailboxStatus;
    use super::*;

    fn scan(alias: &str, rows: Vec<MailboxStatus>) -> IdentityScan {
        IdentityScan {
            alias: alias.to_string(),
            rows,
            seeded: false,
            unparsed_bodystructures: 0,
        }
    }

    #[test]
    fn report_prefixes_each_row_with_its_identity_and_lists_failures() {
        let (row, _) = status::diff(
            "INBOX",
            7,
            &std::collections::HashSet::from([1]),
            &std::collections::HashSet::new(),
            "t",
        );
        let total = status::total(std::slice::from_ref(&row), "t");

        let report = render_report(
            &[scan("work", vec![row, total])],
            &[("home".to_string(), "boom".to_string())],
        );

        let lines: Vec<&str> = report.lines().collect();
        assert!(lines[0].starts_with("identity\tmailbox\tuidvalidity"));
        assert!(lines[1].starts_with("work\tINBOX\t7\t1\t0\t1\t"));
        assert!(lines[2].starts_with("work\t*\t-\t"));
        assert_eq!(lines[3], "# failed\thome\tboom");
    }
}
