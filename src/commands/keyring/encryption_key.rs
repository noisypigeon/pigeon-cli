use serde::{Deserialize, Serialize};

/// A configured symmetric encryption key's non-secret metadata (ADR-0026).
/// The secret key material itself lives in the OS keychain, keyed by
/// `alias` -- see `crate::core::keyring::credentials` -- exactly like every
/// other secret this codebase stores.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EncryptionKey {
    pub alias: String,
    /// RFC3339, set once at creation; never updated by `modify` -- rotating
    /// the secret changes the key, not when the entry was made.
    pub created_at: String,
}

impl crate::core::keyring::KeyringEntry for EncryptionKey {
    fn alias(&self) -> &str {
        &self.alias
    }
    fn kind(&self) -> &'static str {
        "encryption-key"
    }
    fn detail(&self) -> String {
        format!("created {}", self.created_at)
    }
}
