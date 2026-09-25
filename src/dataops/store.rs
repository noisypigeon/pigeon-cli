use serde::{Deserialize, Serialize};

/// A single configured S3-compatible bucket-config's non-secret metadata.
/// The secret access key lives in the OS keychain, keyed by `alias` -- see
/// `crate::keyring::credentials`. Metadata persistence itself lives in
/// `crate::keyring::store` (ADR-0022) -- this struct is kept here since
/// `dataops::client` is its main consumer.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BucketConfig {
    pub alias: String,
    pub endpoint: String,
    pub bucket: String,
    pub access_key_id: String,
}
