//! `WizardInput` implementors shared by more than one job type (ADR-0074).
//! Extracted once a second/third job needed the exact same prompt, not
//! written generically up front -- `email_sync::wizard` originated
//! `UploadTargetInput`; `decrypt_files::wizard` originated `ConfirmInput`.
//! Each job keeps any wizard input that's genuinely its own (e.g.
//! `email_sync`'s own `ConcurrencyInput`, which prints a message-count time
//! estimate no other job has data for).
//!
//! Membership here is earned and can be lost: ADR-0131 removed
//! `SourceBucketInput` outright and returned `EncryptionKeyInput` to
//! `email_sync::wizard` when `pull_transform`'s deletion left them with zero
//! and one consumer respectively -- the same un-hoisting ADR-0094 did to
//! `ConcurrencyInput`.

use dialoguer::{Confirm, Input, theme::ColorfulTheme};

use crate::commands::keyring::store::Store;
use crate::core::wizard::WizardInput;

/// Resolves which bucket-config to upload this run's report/log/transcript
/// to -- mandatory on every job type (ADR-0100). Put here directly
/// (rather than originated in one job first) since every job needs it from
/// day one.
pub(crate) struct ReportBucketInput<'a> {
    pub flag: Option<String>,
    pub store: &'a Store,
}

impl WizardInput for ReportBucketInput<'_> {
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
        Err("--report-bucket is required when not running interactively".to_string())
    }
}

/// Resolves whether (and where) to upload (originally `email_sync`'s
/// `RemoteOutputInput`, ADR-0021 §5 amendment): `Some(alias)` if the flag is
/// given (validated by the caller); on a TTY if omitted, asks whether to
/// upload at all and, if so, reuses `Store::prompt_select_bucket` (ADR-0022
/// -- scoped to bucket-configs only) to pick among `store`'s configured
/// bucket-configs; if omitted and non-interactive, silently returns `None`
/// (skip upload) -- there was already a safe default before this prompt
/// existed.
pub(crate) struct UploadTargetInput<'a> {
    pub flag: Option<String>,
    pub store: &'a Store,
}

impl WizardInput for UploadTargetInput<'_> {
    type Value = Option<String>;

    fn flag_value(&self) -> Option<Result<Option<String>, String>> {
        self.flag.clone().map(|alias| Ok(Some(alias)))
    }

    fn prompt(&self) -> Result<Option<String>, String> {
        let upload = Confirm::with_theme(&ColorfulTheme::default())
            .with_prompt("Upload to a bucket-config?")
            .default(false)
            .interact()
            .map_err(|err| format!("failed to read confirmation: {err}"))?;
        if !upload {
            return Ok(None);
        }
        match self.store.prompt_select_bucket() {
            Ok(bucket_config) => Ok(Some(bucket_config.alias.clone())),
            Err(message) => {
                println!("{message}");
                Ok(None)
            }
        }
    }

    fn non_interactive_fallback(&self) -> Result<Option<String>, String> {
        Ok(None)
    }
}

/// The machine's available core count, or `4` if it can't be determined --
/// shared by every job whose per-item work is CPU-bound (hashing,
/// decompression, decryption) rather than I/O-bound, where a flat `4`
/// (tuned for IMAP/S3-bound jobs) is the wrong default. Originated in
/// `deduplicate/wizard.rs` (ADR-0088); hoisted here once two more jobs
/// needed the identical logic (ADR-0090, this codebase's usual "duplicate
/// until the third consumer" precedent). Still three consumers after
/// ADR-0131: `deduplicate`, `decrypt_files`, and `transform`.
pub(crate) fn default_concurrency() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
}

/// Resolves a plain concurrency value with no time-estimate table, with a
/// cores-based interactive default via `default_concurrency()` for
/// CPU-bound jobs. `--concurrency <N>` and the non-interactive fallback
/// are unchanged (ADR-0088/ADR-0090), except `--concurrency` is now clamped
/// against the host's core count (ADR-0127): a real `transform` run with
/// `--concurrency 50` on a 14-core machine oversubscribed both process count
/// and (via `ffmpeg`'s own per-process threading) CPU threads at once,
/// OOM-killing the host. `CONCURRENCY_CLAMP_MULTIPLIER` leaves headroom for
/// deliberate, modest oversubscription of a partly-I/O-bound pipeline
/// without allowing a pathological value through -- a warning, not a hard
/// error, so an existing non-interactive invocation never starts failing
/// outright (same posture as `UploadConcurrencyInput`'s doc comment,
/// ADR-0091 §3).
const CONCURRENCY_CLAMP_MULTIPLIER: usize = 2;

pub(crate) struct CpuConcurrencyInput {
    pub flag: Option<usize>,
}

impl WizardInput for CpuConcurrencyInput {
    type Value = usize;

    fn flag_value(&self) -> Option<Result<usize, String>> {
        self.flag.map(|value| {
            let value = value.max(1);
            let ceiling = default_concurrency() * CONCURRENCY_CLAMP_MULTIPLIER;
            if value > ceiling {
                tracing::warn!(
                    requested = value,
                    clamped_to = ceiling,
                    "--concurrency exceeds this host's core-count-based safe ceiling; clamping"
                );
                Ok(ceiling)
            } else {
                Ok(value)
            }
        })
    }

    fn prompt(&self) -> Result<usize, String> {
        let value = Input::<usize>::new()
            .with_prompt("Concurrency")
            .default(default_concurrency())
            .interact_text()
            .map_err(|err| format!("failed to read concurrency: {err}"))?;
        Ok(value.max(1))
    }

    fn non_interactive_fallback(&self) -> Result<usize, String> {
        Err("--concurrency is required when not running interactively".to_string())
    }
}

/// Flat default for `UploadConcurrencyInput` (ADR-0091 §3) -- deliberately
/// *not* `default_concurrency()` (core count): the real production run
/// showed the upload phase bottlenecked by per-file round-trip latency,
/// not CPU (median 0.17s/file, p99 1.2s, only 8 uploads in flight,
/// ~30 files/sec on small files) -- a cores-based default would be
/// actively wrong here, the same reasoning ADR-0090 used to deliberately
/// leave `sort`/`email-sync`/`email-pull`'s *primary* concurrency alone.
const UPLOAD_CONCURRENCY_DEFAULT: usize = 16;

/// Resolves the upload phase's own concurrency, independent of whatever
/// `CpuConcurrencyInput` (or a job's own local `ConcurrencyInput`) resolves
/// for a job's primary (download/hash/transform/fetch) work (ADR-0091 §3)
/// -- the upload phase is network-RTT-bound, not CPU-bound, so tying it to
/// core count or to a flat value tuned for IMAP is wrong in either
/// direction. Unlike those non-interactive fallbacks (which error,
/// requiring `--concurrency`), this falls back to
/// `UPLOAD_CONCURRENCY_DEFAULT` rather than erroring: it's a new flag being
/// added to commands that already run unattended in scripts/cron today,
/// and requiring it non-interactively would break every existing
/// non-interactive invocation that predates this flag.
pub(crate) struct UploadConcurrencyInput {
    pub flag: Option<usize>,
}

impl WizardInput for UploadConcurrencyInput {
    type Value = usize;

    fn flag_value(&self) -> Option<Result<usize, String>> {
        self.flag.map(|value| Ok(value.max(1)))
    }

    fn prompt(&self) -> Result<usize, String> {
        let value = Input::<usize>::new()
            .with_prompt("Upload concurrency")
            .default(UPLOAD_CONCURRENCY_DEFAULT)
            .interact_text()
            .map_err(|err| format!("failed to read upload concurrency: {err}"))?;
        Ok(value.max(1))
    }

    fn non_interactive_fallback(&self) -> Result<usize, String> {
        Ok(UPLOAD_CONCURRENCY_DEFAULT)
    }
}

/// The final "proceed?" gate, identical across every job: `--non-interactive`
/// skips it outright; otherwise prompts on a TTY, and errors outside one
/// (there's no sane way to read a yes/no answer from a pipe without an
/// established convention for it here -- requiring `--non-interactive` for a
/// non-interactive run is simpler and safer than inventing one solely for
/// this prompt).
pub(crate) struct ConfirmInput {
    pub non_interactive: bool,
}

impl WizardInput for ConfirmInput {
    type Value = bool;

    fn flag_value(&self) -> Option<Result<bool, String>> {
        self.non_interactive.then_some(Ok(true))
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
            "confirmation is required when not running interactively (pass --non-interactive to skip)"
                .to_string(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_concurrency_is_at_least_one() {
        assert!(default_concurrency() >= 1);
    }

    #[test]
    fn cpu_concurrency_input_flag_value_overrides_the_default() {
        // 1 is always within the core-count-based clamp ceiling, regardless
        // of how many cores the machine running this test actually has.
        let input = CpuConcurrencyInput { flag: Some(1) };
        assert_eq!(input.flag_value(), Some(Ok(1)));
    }

    #[test]
    fn cpu_concurrency_input_requires_a_flag_when_not_interactive() {
        let input = CpuConcurrencyInput { flag: None };
        assert!(input.non_interactive_fallback().is_err());
    }

    #[test]
    fn cpu_concurrency_input_clamps_a_flag_value_far_above_the_core_count() {
        let ceiling = default_concurrency() * CONCURRENCY_CLAMP_MULTIPLIER;
        let input = CpuConcurrencyInput {
            flag: Some(ceiling + 1000),
        };
        assert_eq!(input.flag_value(), Some(Ok(ceiling)));
    }

    #[test]
    fn cpu_concurrency_input_does_not_clamp_a_flag_value_at_the_ceiling() {
        let ceiling = default_concurrency() * CONCURRENCY_CLAMP_MULTIPLIER;
        let input = CpuConcurrencyInput {
            flag: Some(ceiling),
        };
        assert_eq!(input.flag_value(), Some(Ok(ceiling)));
    }

    #[test]
    fn upload_concurrency_input_flag_value_overrides_the_default() {
        let input = UploadConcurrencyInput { flag: Some(32) };
        assert_eq!(input.flag_value(), Some(Ok(32)));
    }

    #[test]
    fn upload_concurrency_input_falls_back_to_a_default_when_not_interactive() {
        let input = UploadConcurrencyInput { flag: None };
        assert_eq!(
            input.non_interactive_fallback(),
            Ok(UPLOAD_CONCURRENCY_DEFAULT)
        );
    }
}
