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
