use serde::{Deserialize, Serialize};

/// A single configured S3-compatible bucket-config's metadata, including its
/// secret access key as a plain field -- `keyring.toml` is this entry's sole
/// storage since CLI-0135 retired the OS keychain, protected only by
/// `Store::save`'s `0600` file permissions. Deliberately has no
/// `#[serde(default)]` on `secret_key`: a pre-CLI-0135 entry with no secret
/// field fails to parse loudly rather than silently loading an empty secret.
/// Metadata persistence itself lives in `crate::commands::keyring::store`
/// (CLI-0022) -- this struct is kept here since `client` (this module's
/// sibling) is its main consumer.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BucketConfig {
    pub alias: String,
    pub endpoint: String,
    pub bucket: String,
    pub access_key_id: String,
    /// S3 secret access key. See
    /// `crate::core::keyring::credentials::resolve_secret` for the one
    /// remaining override (CLI-0087's `PIGEON_SECRET_<ALIAS>`).
    pub secret_key: String,
}

impl crate::core::keyring::KeyringEntry for BucketConfig {
    fn alias(&self) -> &str {
        &self.alias
    }
    fn kind(&self) -> &'static str {
        "bucket"
    }
    fn detail(&self) -> String {
        format!("{} ({})", self.endpoint, self.bucket)
    }
}
