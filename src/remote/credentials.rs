/// Distinct from `email::credentials`'s `"pigeon"` service name so a remote
/// and an email identity that happen to share an alias (e.g. both called
/// "email") can never collide in the OS keychain.
const SERVICE_NAME: &str = "pigeon-remote";

/// Stores `secret` (the S3 secret access key) in the OS-native secure
/// credential store, keyed by remote `alias`. Per ADR-0009, this is the only
/// place a secret access key is ever written -- never to `remotes.toml`.
pub fn set_secret(alias: &str, secret: &str) -> Result<(), String> {
    let entry = keyring::Entry::new(SERVICE_NAME, alias)
        .map_err(|err| format!("failed to open keychain entry for '{alias}': {err}"))?;
    entry
        .set_password(secret)
        .map_err(|err| format!("failed to store secret for '{alias}': {err}"))
}

/// Reads back the secret stored for `alias` via `set_secret`.
pub fn get_secret(alias: &str) -> Result<String, String> {
    let entry = keyring::Entry::new(SERVICE_NAME, alias)
        .map_err(|err| format!("failed to open keychain entry for '{alias}': {err}"))?;
    entry
        .get_password()
        .map_err(|err| format!("failed to read secret for '{alias}': {err}"))
}

/// Removes the stored secret for `alias`, if any. A missing entry
/// (`keyring::Error::NoEntry`) is treated as success, not a failure -- used
/// both to roll back a partially completed `configure`/`edit` and by
/// `remote remove`, which should cleanly no-op if there was never a secret
/// to begin with (e.g. a hand-edited `remotes.toml` entry).
pub fn delete_secret(alias: &str) -> Result<(), String> {
    let entry = keyring::Entry::new(SERVICE_NAME, alias)
        .map_err(|err| format!("failed to open keychain entry for '{alias}': {err}"))?;
    match entry.delete_credential() {
        Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
        Err(err) => Err(format!("failed to delete secret for '{alias}': {err}")),
    }
}
