use serde::{Deserialize, Serialize};

/// A single configured S3-compatible bucket-config's non-secret metadata.
/// The secret access key lives in the OS keychain, keyed by `alias` -- see
/// `crate::core::keyring::credentials`. Metadata persistence itself lives in
/// `crate::commands::keyring::store` (ADR-0022) -- this struct is kept here
/// since `client` (this module's sibling) is its main consumer.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BucketConfig {
    pub alias: String,
    pub endpoint: String,
    pub bucket: String,
    pub access_key_id: String,
    /// Alias of the encryption key this bucket encrypts with by default
    /// (ADR-0027). `None` means no default -- every job run gets asked
    /// explicitly with no bucket-level nudge either way.
    #[serde(default)]
    pub encryption_key_alias: Option<String>,
}

impl crate::core::keyring::KeyringEntry for BucketConfig {
    fn alias(&self) -> &str {
        &self.alias
    }
    fn kind(&self) -> &'static str {
        "bucket"
    }
    fn detail(&self) -> String {
        match &self.encryption_key_alias {
            Some(alias) => format!(
                "{} ({}), encrypts with '{alias}'",
                self.endpoint, self.bucket
            ),
            None => format!("{} ({})", self.endpoint, self.bucket),
        }
    }
}
