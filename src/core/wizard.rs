use std::io::{BufRead, IsTerminal};

use dialoguer::{Confirm, Password, theme::ColorfulTheme};

/// One value a wizard needs, resolved either from a CLI flag or an
/// interactive prompt. Captures a pattern that appeared five times,
/// independently hand-written, in this codebase's job wizard before
/// ADR-0023 (`resolve_identities`, `resolve_local_output`,
/// `resolve_remote_output`, `resolve_concurrency`, `confirm_and_proceed`).
pub(crate) trait WizardInput {
    type Value;

    /// `None` if the flag was omitted (fall through to `prompt`/
    /// `non_interactive_fallback`); `Some(Err(_))` if it was given but
    /// invalid (surfaced immediately -- an invalid flag never falls
    /// through to an interactive prompt, e.g. an unknown identity alias).
    fn flag_value(&self) -> Option<Result<Self::Value, String>>;

    /// Interactively resolves the value; only ever called on a real TTY.
    fn prompt(&self) -> Result<Self::Value, String>;

    /// Resolves the value when the flag was omitted and stdin isn't a
    /// terminal. `Err` for a required input (no prior default exists);
    /// `Ok(default)` for an optional one (a safe default already existed
    /// before this input gained a prompt).
    fn non_interactive_fallback(&self) -> Result<Self::Value, String>;

    /// The shared resolution algorithm every wizard input follows, given
    /// once here so implementors only define the three methods above.
    fn resolve(&self) -> Result<Self::Value, String> {
        if let Some(result) = self.flag_value() {
            return result;
        }
        if std::io::stdin().is_terminal() {
            self.prompt()
        } else {
            self.non_interactive_fallback()
        }
    }
}

/// Reads a secret. Masked and interactive on a real TTY; falls back to a
/// plain line read from stdin otherwise, so a secret can be piped in (e.g.
/// from a password manager).
pub fn read_secret(prompt: &str) -> Result<String, String> {
    if std::io::stdin().is_terminal() {
        Password::new()
            .with_prompt(prompt)
            .interact()
            .map_err(|err| format!("failed to read secret: {err}"))
    } else {
        let mut line = String::new();
        std::io::stdin()
            .lock()
            .read_line(&mut line)
            .map_err(|err| format!("failed to read secret from stdin: {err}"))?;
        Ok(line.trim_end_matches(['\n', '\r']).to_string())
    }
}

/// Asks a yes/no question. Interactive on a real TTY; falls back to reading
/// a plain `y`/`n` line from stdin otherwise.
pub fn confirm(prompt: &str, default: bool) -> Result<bool, String> {
    if std::io::stdin().is_terminal() {
        Confirm::with_theme(&ColorfulTheme::default())
            .with_prompt(prompt)
            .default(default)
            .interact()
            .map_err(|err| format!("failed to read confirmation: {err}"))
    } else {
        let mut line = String::new();
        std::io::stdin()
            .lock()
            .read_line(&mut line)
            .map_err(|err| format!("failed to read confirmation from stdin: {err}"))?;
        Ok(match line.trim().to_ascii_lowercase().as_str() {
            "y" | "yes" => true,
            "n" | "no" => false,
            _ => default,
        })
    }
}
