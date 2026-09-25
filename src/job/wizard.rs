use std::io::IsTerminal;
use std::path::PathBuf;

use dialoguer::{Confirm, Input, MultiSelect, theme::ColorfulTheme};

use crate::commands::print_table;
use crate::email::identity::Identity;
use crate::job::email_sync::IdentityManifestSummary;
use crate::keyring::store::Store;

/// Resolves which identities to run against: `--identities` if given (every
/// alias must already exist), an interactive `MultiSelect` if omitted and
/// stdin is a terminal, or a hard error otherwise.
///
/// Per ADR-0021 §8's narrow `--yes` semantics: `--yes` only skips the final
/// proceed confirmation. A missing `--identities` outside a TTY is always
/// an error, regardless of `--yes` -- silently defaulting to "every
/// configured identity" would be a much worse failure mode for a scripted/
/// cron invocation than a fast, explicit error naming the missing flag.
pub(crate) fn resolve_identities(
    store: &Store,
    identities: Option<Vec<String>>,
) -> Result<Vec<Identity>, String> {
    match identities {
        Some(aliases) => aliases
            .into_iter()
            .map(|alias| {
                store
                    .email_identities()
                    .find(|identity| identity.alias == alias)
                    .cloned()
                    .ok_or_else(|| format!("no identity with alias '{alias}'"))
            })
            .collect(),
        None => {
            if !std::io::stdin().is_terminal() {
                return Err("--identities is required when not running interactively".to_string());
            }
            select_identities_interactively(store)
        }
    }
}

fn select_identities_interactively(store: &Store) -> Result<Vec<Identity>, String> {
    let all: Vec<&Identity> = store.email_identities().collect();
    if all.is_empty() {
        return Err("no identities configured; run 'pigeon keyring add email' first".to_string());
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

/// The shared local-output root used when `--local-output` is omitted and
/// there's no interactive prompt to fall back to a chosen value (or the
/// prompt itself is seeded with this as its editable default).
fn default_local_output() -> PathBuf {
    std::env::temp_dir().join("pigeon-job")
}

/// Resolves the shared local-output root (ADR-0021 §5 amendment):
/// `--local-output` if given; an editable `Input` prompt (default
/// `$TMPDIR/pigeon-job`) on a TTY if omitted; that same default silently,
/// no prompt, if omitted and non-interactive -- unlike `resolve_identities`/
/// `resolve_concurrency`, an omitted value here is never an error, since it
/// already had a safe default before this prompt existed (ADR-0021 §8).
pub(crate) fn resolve_local_output(local_output: Option<PathBuf>) -> Result<PathBuf, String> {
    if let Some(path) = local_output {
        return Ok(path);
    }
    let default = default_local_output();
    if !std::io::stdin().is_terminal() {
        return Ok(default);
    }
    let value = Input::<String>::new()
        .with_prompt("Local directory to stage and store output under")
        .default(default.display().to_string())
        .interact_text()
        .map_err(|err| format!("failed to read local output directory: {err}"))?;
    Ok(PathBuf::from(value))
}

/// Resolves whether (and where) to upload (ADR-0021 §5 amendment):
/// `Some(alias)` if `--remote-output` is given (validated by the caller,
/// unchanged); on a TTY if omitted, asks whether to upload at all and, if
/// so, reuses `Store::prompt_select_bucket` (ADR-0022 -- scoped to
/// bucket-configs only, ignoring any configured email identities) to pick
/// among `store`'s configured bucket-configs (auto-selecting the only one
/// if there's exactly one, or printing `prompt_select_bucket`'s own "run
/// keyring add bucket" message and skipping upload if there are none); if
/// omitted and non-interactive, silently returns `None` (skip upload) --
/// same "no error, safe prior default" reasoning as `resolve_local_output`.
pub(crate) fn resolve_remote_output(
    remote_output: Option<String>,
    store: &Store,
) -> Result<Option<String>, String> {
    if let Some(alias) = remote_output {
        return Ok(Some(alias));
    }
    if !std::io::stdin().is_terminal() {
        return Ok(None);
    }
    let upload = Confirm::with_theme(&ColorfulTheme::default())
        .with_prompt("Upload to a bucket-config?")
        .default(false)
        .interact()
        .map_err(|err| format!("failed to read confirmation: {err}"))?;
    if !upload {
        return Ok(None);
    }
    match store.prompt_select_bucket() {
        Ok(bucket_config) => Ok(Some(bucket_config.alias.clone())),
        Err(message) => {
            println!("{message}");
            Ok(None)
        }
    }
}

/// Prints a per-identity manifest summary table (ADR-0021 §5).
pub(crate) fn print_manifest_summary(summaries: &[IdentityManifestSummary]) {
    let rows: Vec<Vec<String>> = summaries
        .iter()
        .map(|summary| {
            vec![
                summary.alias.clone(),
                summary.mailboxes.to_string(),
                summary.pending_messages.to_string(),
                format_bytes(summary.pending_bytes),
            ]
        })
        .collect();
    print_table(&["IDENTITY", "MAILBOXES", "PENDING", "SIZE"], &rows);
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
pub(crate) fn candidate_concurrencies(total_pending: usize) -> Vec<usize> {
    let candidates = [1, 2, 4, 8, 16];
    let capped: Vec<usize> = candidates
        .into_iter()
        .filter(|candidate| *candidate <= total_pending.max(1))
        .collect();
    if capped.is_empty() { vec![1] } else { capped }
}

pub(crate) fn estimate_seconds(pending_messages: usize, concurrency: usize) -> f64 {
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
/// `resolve_identities`.
pub(crate) fn resolve_concurrency(
    concurrency: Option<usize>,
    total_pending_messages: usize,
) -> Result<usize, String> {
    match concurrency {
        Some(value) => Ok(value.max(1)),
        None => {
            if !std::io::stdin().is_terminal() {
                return Err("--concurrency is required when not running interactively".to_string());
            }
            print_concurrency_estimate(total_pending_messages);
            let value = Input::<usize>::new()
                .with_prompt("Concurrency")
                .default(4)
                .interact_text()
                .map_err(|err| format!("failed to read concurrency: {err}"))?;
            Ok(value.max(1))
        }
    }
}

/// The final "proceed?" gate. `--yes` skips it outright; otherwise prompts
/// on a TTY, and errors outside one (there is no sane way to read a
/// yes/no answer from a pipe without an established convention for it here
/// -- unlike `Password`/`Confirm` elsewhere in this codebase, which do have
/// one; requiring `--yes` for a non-interactive run is simpler and safer
/// than inventing a new one solely for this prompt).
pub(crate) fn confirm_and_proceed(yes: bool) -> Result<bool, String> {
    if yes {
        return Ok(true);
    }
    if !std::io::stdin().is_terminal() {
        return Err(
            "confirmation is required when not running interactively (pass --yes to skip)"
                .to_string(),
        );
    }
    Confirm::with_theme(&ColorfulTheme::default())
        .with_prompt("Proceed?")
        .default(true)
        .interact()
        .map_err(|err| format!("failed to read confirmation: {err}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_local_output_is_under_the_os_temp_dir() {
        let path = default_local_output();
        assert!(path.starts_with(std::env::temp_dir()));
        assert_eq!(path.file_name().unwrap(), "pigeon-job");
    }

    #[test]
    fn resolve_local_output_returns_given_path_unchanged() {
        let given = PathBuf::from("/explicit/path");
        assert_eq!(resolve_local_output(Some(given.clone())).unwrap(), given);
    }

    #[test]
    fn resolve_remote_output_returns_given_alias_unchanged() {
        let store = Store::default();
        assert_eq!(
            resolve_remote_output(Some("backup".to_string()), &store).unwrap(),
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
