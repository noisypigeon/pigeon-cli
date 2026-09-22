const SERVICE_NAME: &str = "pigeon";

/// Stores `secret` in the OS-native secure credential store (macOS Keychain,
/// Linux Secret Service, Windows Credential Manager), keyed by `alias`.
/// Per ADR-0003, this is the only place an app/bridge password is ever
/// written -- never to the identity metadata file.
pub fn set_secret(alias: &str, secret: &str) -> Result<(), String> {
    let entry = keyring::Entry::new(SERVICE_NAME, alias)
        .map_err(|err| format!("failed to open keychain entry for '{alias}': {err}"))?;
    entry
        .set_password(secret)
        .map_err(|err| format!("failed to store secret for '{alias}': {err}"))
}

/// Removes the stored secret for `alias`, if any. Used to roll back a
/// partially completed `authenticate` if a later step fails.
pub fn delete_secret(alias: &str) -> Result<(), String> {
    let entry = keyring::Entry::new(SERVICE_NAME, alias)
        .map_err(|err| format!("failed to open keychain entry for '{alias}': {err}"))?;
    entry
        .delete_credential()
        .map_err(|err| format!("failed to delete secret for '{alias}': {err}"))
}
