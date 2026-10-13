//! Generates the rclone config file pigeon passes via `--config` to every
//! invocation (ADR-0136) -- the *only* config file `rclone` is ever given.
//! Never reads an existing file, never consults `RCLONE_CONFIG` or rclone's
//! own default config-file path: every rclone-backed job is sandboxed to
//! exactly the buckets configured in pigeon's own keyring, nothing else --
//! not a separately-provisioned deployment `rclone.conf`, not the user's own
//! general-purpose one.

use std::path::{Path, PathBuf};

use crate::commands::keyring::store::Store;

const RCLONE_CONFIG_FILE_NAME: &str = "rclone.conf";

/// Sibling to `Store::default_path()`'s `keyring.toml` -- same config
/// directory, so the generated file is one well-known, inspectable artifact
/// rather than a new one per run.
pub(crate) fn rclone_config_path() -> Result<PathBuf, String> {
    let keyring_path = Store::default_path()?;
    let dir = keyring_path.parent().ok_or_else(|| {
        format!(
            "could not determine a parent directory for {}",
            keyring_path.display()
        )
    })?;
    Ok(dir.join(RCLONE_CONFIG_FILE_NAME))
}

/// Writes one `[alias]` section per configured `BucketConfig` to `path`,
/// keyed directly by alias -- aliases are already globally unique
/// (`Store::contains_alias`, ADR-0022), so no two-section indirection and no
/// collision handling is needed, unlike the external terraform-managed
/// config this supersedes. Regenerated fresh from the current keyring state
/// every time this is called (no existing file at `path` is ever read
/// first), so a `pigeon keyring modify bucket` takes effect on the very next
/// job run with no cache to invalidate.
pub(crate) fn write_sandboxed_config(store: &Store, path: &Path) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|err| format!("failed to create {}: {err}", parent.display()))?;
    }

    let mut contents = String::new();
    for bucket_config in store.bucket_configs() {
        contents.push_str(&format!(
            "[{}]\ntype = s3\nprovider = Other\naccess_key_id = {}\nsecret_access_key = {}\nendpoint = {}\nacl = private\nno_check_bucket = true\n\n",
            bucket_config.alias,
            bucket_config.access_key_id,
            bucket_config.secret_key,
            bucket_config.endpoint,
        ));
    }

    std::fs::write(path, contents)
        .map_err(|err| format!("failed to write {}: {err}", path.display()))?;

    // This file carries every configured bucket's secret access key in
    // plaintext, same trust model as `keyring.toml` itself (ADR-0135).
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
            .map_err(|err| format!("failed to set permissions on {}: {err}", path.display()))?;
    }

    Ok(())
}

/// The rclone remote address for a configured bucket alias -- always the
/// whole bucket's root. Unlike the raw `remote:path` strings this
/// supersedes, there is no sub-path affordance: a bucket-config alias
/// addresses one entire bucket, matching `deduplicate`'s existing
/// `--source-bucket`/`--destination-bucket` model.
pub(crate) fn remote_address(alias: &str) -> String {
    format!("{alias}:")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::keyring::bucket::store::BucketConfig;
    use crate::commands::keyring::store::Entry;

    fn sample_bucket(alias: &str) -> BucketConfig {
        BucketConfig {
            alias: alias.to_string(),
            endpoint: "https://nyc3.digitaloceanspaces.com".to_string(),
            bucket: "my-bucket".to_string(),
            access_key_id: "AKID".to_string(),
            encryption_key_alias: None,
            secret_key: "shh".to_string(),
        }
    }

    #[test]
    fn remote_address_appends_a_trailing_colon() {
        assert_eq!(remote_address("backup"), "backup:");
    }

    #[test]
    fn write_sandboxed_config_with_no_bucket_configs_writes_an_empty_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rclone.conf");
        let store = Store::default();
        write_sandboxed_config(&store, &path).unwrap();
        let contents = std::fs::read_to_string(&path).unwrap();
        assert!(contents.is_empty());
    }

    #[test]
    fn write_sandboxed_config_writes_one_section_per_bucket_keyed_by_alias() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rclone.conf");
        let mut store = Store::default();
        store.push(Entry::Bucket(sample_bucket("backup")));
        write_sandboxed_config(&store, &path).unwrap();
        let contents = std::fs::read_to_string(&path).unwrap();
        assert!(contents.contains("[backup]"));
        assert!(contents.contains("type = s3"));
        assert!(contents.contains("provider = Other"));
        assert!(contents.contains("access_key_id = AKID"));
        assert!(contents.contains("secret_access_key = shh"));
        assert!(contents.contains("endpoint = https://nyc3.digitaloceanspaces.com"));
        // No terraform-style second, bucket-named indirection section --
        // aliases are already globally unique, so one section per bucket
        // config is sufficient.
        assert!(!contents.contains("[my-bucket]"));
    }

    #[test]
    fn write_sandboxed_config_never_reads_an_existing_file_first() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rclone.conf");
        std::fs::write(&path, "[unrelated]\ntype = sftp\n").unwrap();

        let store = Store::default();
        write_sandboxed_config(&store, &path).unwrap();
        let contents = std::fs::read_to_string(&path).unwrap();
        assert!(
            !contents.contains("unrelated"),
            "write_sandboxed_config must overwrite, never merge with, any pre-existing file"
        );
    }

    #[test]
    #[cfg(unix)]
    fn write_sandboxed_config_sets_0600_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rclone.conf");
        let mut store = Store::default();
        store.push(Entry::Bucket(sample_bucket("backup")));
        write_sandboxed_config(&store, &path).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }
}
