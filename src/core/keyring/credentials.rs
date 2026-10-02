/// One keychain service for every kind of secret this CLI stores (ADR-0022)
/// -- replaces `email::credentials`'s `"pigeon"` and `dataops::credentials`'s
/// `"pigeon-dataops"`. Safe to merge now specifically because
/// `keyring::store::Store::contains_alias` enforces alias uniqueness across
/// both kinds: the entire reason dataops picked a separate service name in
/// the first place (so a same-alias identity and bucket-config could never
/// collide in the keychain) is now structurally moot.
const SERVICE_NAME: &str = "pigeon";

/// Stores `secret` in the OS-native secure credential store (macOS Keychain,
/// Linux Secret Service, Windows Credential Manager), keyed by `alias`.
pub fn set_secret(alias: &str, secret: &str) -> Result<(), String> {
    let entry = keyring::Entry::new(SERVICE_NAME, alias)
        .map_err(|err| format!("failed to open keychain entry for '{alias}': {err}"))?;
    entry
        .set_password(secret)
        .map_err(|err| format!("failed to store secret for '{alias}': {err}"))
}

/// Env var name checked by `get_secret` before it falls back to the OS
/// keyring (ADR-0087) -- `PIGEON_SECRET_` plus `alias` uppercased, with every
/// character outside `[A-Z0-9_]` replaced by `_`. Lets a secret be injected
/// directly (no OS keyring involved at all), which is what makes a
/// non-interactive or restarted container usable: the Linux kernel-keyutils
/// backend (ADR-0085) doesn't survive a process/session reset, and a
/// container restart is exactly that.
fn env_var_name(alias: &str) -> String {
    let mut name = String::from("PIGEON_SECRET_");
    for ch in alias.chars() {
        let upper = ch.to_ascii_uppercase();
        name.push(if upper.is_ascii_alphanumeric() || upper == '_' {
            upper
        } else {
            '_'
        });
    }
    name
}

/// Reads back the secret stored for `alias` via `set_secret`, unless
/// `env_var_name(alias)` is set in the environment, in which case that value
/// is returned directly and the OS keyring is never consulted (ADR-0087).
pub fn get_secret(alias: &str) -> Result<String, String> {
    if let Ok(secret) = std::env::var(env_var_name(alias)) {
        return Ok(secret);
    }
    let entry = keyring::Entry::new(SERVICE_NAME, alias)
        .map_err(|err| format!("failed to open keychain entry for '{alias}': {err}"))?;
    entry
        .get_password()
        .map_err(|err| format!("failed to read secret for '{alias}': {err}"))
}

/// Removes the stored secret for `alias`, if any. A missing entry
/// (`keyring::Error::NoEntry`) is treated as success, not a failure -- used
/// both to roll back a partially completed `keyring add`/`modify` and by
/// `keyring delete`, which should cleanly no-op if there was never a secret
/// to begin with (e.g. a hand-edited `keyring.toml` entry).
pub fn delete_secret(alias: &str) -> Result<(), String> {
    let entry = keyring::Entry::new(SERVICE_NAME, alias)
        .map_err(|err| format!("failed to open keychain entry for '{alias}': {err}"))?;
    match entry.delete_credential() {
        Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
        Err(err) => Err(format!("failed to delete secret for '{alias}': {err}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_var_name_uppercases_and_sanitizes_the_alias() {
        assert_eq!(env_var_name("gmail-work"), "PIGEON_SECRET_GMAIL_WORK");
        assert_eq!(env_var_name("My.Bucket"), "PIGEON_SECRET_MY_BUCKET");
    }

    #[test]
    fn get_secret_prefers_the_env_var_over_the_os_keyring() {
        let alias = "adr-0087-test-alias";
        let var = env_var_name(alias);
        // SAFETY: tests run single-threaded within this process for this env var
        // (no other test reads/writes a PIGEON_SECRET_* var concurrently).
        unsafe {
            std::env::set_var(&var, "injected-secret");
        }
        let result = get_secret(alias);
        unsafe {
            std::env::remove_var(&var);
        }
        assert_eq!(result.unwrap(), "injected-secret");
    }
}
