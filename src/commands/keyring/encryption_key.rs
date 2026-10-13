use serde::{Deserialize, Serialize};

/// A configured symmetric encryption key's metadata, including its secret
/// key material as a plain field (ADR-0026, secret storage per ADR-0135) --
/// `keyring.toml` is this entry's sole storage since ADR-0135 retired the OS
/// keychain, protected only by `Store::save`'s `0600` file permissions.
/// Deliberately has no `#[serde(default)]` on `key_hex`: a pre-ADR-0135
/// entry with no secret field fails to parse loudly rather than silently
/// loading an empty key.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EncryptionKey {
    pub alias: String,
    /// RFC3339, set once at creation; never updated by `modify` -- rotating
    /// the secret changes the key, not when the entry was made.
    pub created_at: String,
    /// 64 hex characters (32 bytes / 256 bits). See
    /// `crate::core::keyring::credentials::resolve_secret` for the one
    /// remaining override (ADR-0087's `PIGEON_SECRET_<ALIAS>`).
    pub key_hex: String,
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
