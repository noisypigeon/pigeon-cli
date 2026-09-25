pub mod credentials;

/// Behavior shared by every kind of secret this CLI manages under one
/// `keyring.toml` + one keychain service (ADR-0022), implemented by
/// `commands::keyring::email::Identity`, `commands::keyring::bucket::
/// BucketConfig`, and the concrete `Entry` enum that wraps both
/// (`commands::keyring::store::Entry`) for storage (ADR-0023).
pub(crate) trait KeyringEntry {
    fn alias(&self) -> &str;
    /// `"email"` / `"bucket"` -- matches the serde tag value used to
    /// persist the concrete `Entry` enum.
    fn kind(&self) -> &'static str;
    /// A one-line summary for `list`/`prompt_select` labels.
    fn detail(&self) -> String;
}
