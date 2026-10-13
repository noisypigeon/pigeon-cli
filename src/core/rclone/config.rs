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

/// A configured bucket alias plus an optional prefix (sub-directory)
/// within it -- `source` is the whole bucket root, `source:mp4/` is the
/// `mp4/` directory inside it (ADR-0138, restoring the addressing ADR-0136
/// removed). The spelling is deliberately rclone's own `remote:path`, since
/// `write_sandboxed_config` keys each config section by alias: the composed
/// address resolves natively with no further translation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RemoteTarget {
    alias: String,
    /// Normalized by `parse`: no leading `/`, always a trailing `/` when
    /// present, never empty (an empty prefix is `None`). `None` is the
    /// whole bucket root.
    prefix: Option<String>,
}

impl RemoteTarget {
    /// Parses `alias` or `alias:prefix/`, splitting on the **first** colon
    /// and validating the alias half against the keyring exactly as
    /// ADR-0136/ADR-0137 did. A bare alias (or a trailing-colon-only
    /// `alias:`) is the whole bucket root. Because the split takes the
    /// first colon, an alias containing `:` is unaddressable in this form
    /// -- a stated ADR-0138 consequence, not a guarded case.
    pub(crate) fn parse(value: &str, store: &Store) -> Result<RemoteTarget, String> {
        let (alias, raw_prefix) = match value.split_once(':') {
            Some((alias, prefix)) => (alias, prefix),
            None => (value, ""),
        };
        if !store.contains_alias(alias) {
            return Err(format!("no bucket-config named '{alias}'"));
        }
        Ok(RemoteTarget {
            alias: alias.to_string(),
            prefix: normalize_prefix(raw_prefix)?,
        })
    }

    /// The whole bucket root for an alias that is already known-valid --
    /// `deduplicate`, which holds a resolved `BucketConfig` rather than a
    /// raw flag string and keeps whole-bucket addressing (ADR-0138 §2, its
    /// alias-keyed checkpoint identity would need rework to carry a
    /// prefix). No `Store` needed, since there is nothing left to validate.
    pub(crate) fn bucket_root(alias: &str) -> RemoteTarget {
        RemoteTarget {
            alias: alias.to_string(),
            prefix: None,
        }
    }

    /// Builds a target from an already-validated alias plus a prefix to use
    /// when the caller's own value carried none -- `transform`'s
    /// `--input-file-type`-derived default (ADR-0138 §3). A prefix already
    /// present is kept verbatim; the default never overrides it.
    pub(crate) fn with_default_prefix(mut self, default_prefix: &str) -> Result<Self, String> {
        if self.prefix.is_none() {
            self.prefix = normalize_prefix(default_prefix)?;
        }
        Ok(self)
    }

    /// The rclone address: `alias:` for a whole bucket, `alias:prefix/`
    /// otherwise. The trailing `/` is guaranteed whenever a prefix is
    /// present, so a caller appending an object key
    /// (`format!("{}{key}", target.address())`) composes correctly without
    /// having to add a separator itself.
    pub(crate) fn address(&self) -> String {
        match &self.prefix {
            Some(prefix) => format!("{}:{prefix}", self.alias),
            None => format!("{}:", self.alias),
        }
    }
}

/// Trims a raw prefix into `RemoteTarget`'s normalized form: `None` when
/// empty, otherwise no leading `/` and exactly one trailing `/`. A `..`
/// segment is rejected outright -- `rclone` would take it literally, so
/// refusing is clearer than silently escaping the prefix that was asked
/// for.
fn normalize_prefix(raw: &str) -> Result<Option<String>, String> {
    let trimmed = raw.trim_matches('/');
    if trimmed.is_empty() {
        return Ok(None);
    }
    if trimmed.split('/').any(|segment| segment == "..") {
        return Err(format!(
            "invalid prefix '{raw}': a '..' path segment is not allowed"
        ));
    }
    Ok(Some(format!("{trimmed}/")))
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

    fn store_with(aliases: &[&str]) -> Store {
        let mut store = Store::default();
        for alias in aliases {
            store.push(Entry::Bucket(sample_bucket(alias)));
        }
        store
    }

    #[test]
    fn a_bare_alias_addresses_the_whole_bucket_root() {
        let store = store_with(&["backup"]);
        let target = RemoteTarget::parse("backup", &store).unwrap();
        assert_eq!(target.address(), "backup:");
    }

    #[test]
    fn a_trailing_colon_with_no_prefix_is_also_the_bucket_root() {
        let store = store_with(&["backup"]);
        assert_eq!(
            RemoteTarget::parse("backup:", &store).unwrap().address(),
            "backup:"
        );
    }

    #[test]
    fn a_prefix_is_normalized_to_exactly_one_trailing_slash() {
        let store = store_with(&["source"]);
        // With and without the trailing slash, and with a stray leading
        // one, all normalize identically -- so appending an object key to
        // `address()` always composes correctly.
        for spelling in ["source:mp4", "source:mp4/", "source:/mp4/"] {
            assert_eq!(
                RemoteTarget::parse(spelling, &store).unwrap().address(),
                "source:mp4/",
                "spelling {spelling} did not normalize"
            );
        }
    }

    #[test]
    fn a_nested_prefix_keeps_its_interior_separators() {
        let store = store_with(&["source"]);
        assert_eq!(
            RemoteTarget::parse("source:video/mp4/", &store)
                .unwrap()
                .address(),
            "source:video/mp4/"
        );
    }

    #[test]
    fn the_alias_half_is_validated_against_the_keyring() {
        let store = store_with(&["source"]);
        assert_eq!(
            RemoteTarget::parse("nope:mp4/", &store),
            Err("no bucket-config named 'nope'".to_string())
        );
        assert_eq!(
            RemoteTarget::parse("nope", &store),
            Err("no bucket-config named 'nope'".to_string())
        );
    }

    #[test]
    fn a_dot_dot_path_segment_is_rejected() {
        let store = store_with(&["source"]);
        assert!(RemoteTarget::parse("source:../escape/", &store).is_err());
        assert!(RemoteTarget::parse("source:mp4/../other/", &store).is_err());
        // A name merely *containing* dots is fine -- only a whole `..`
        // segment is a traversal attempt.
        assert_eq!(
            RemoteTarget::parse("source:..hidden/", &store)
                .unwrap()
                .address(),
            "source:..hidden/"
        );
    }

    #[test]
    fn with_default_prefix_fills_in_only_an_absent_prefix() {
        let store = store_with(&["source"]);
        let defaulted = RemoteTarget::parse("source", &store)
            .unwrap()
            .with_default_prefix("mp4")
            .unwrap();
        assert_eq!(defaulted.address(), "source:mp4/");

        // An explicit prefix always wins -- the default never overrides it.
        let explicit = RemoteTarget::parse("source:raw-video/", &store)
            .unwrap()
            .with_default_prefix("mp4")
            .unwrap();
        assert_eq!(explicit.address(), "source:raw-video/");
    }

    #[test]
    fn with_an_empty_default_prefix_the_target_stays_the_bucket_root() {
        let store = store_with(&["source"]);
        let target = RemoteTarget::parse("source", &store)
            .unwrap()
            .with_default_prefix("")
            .unwrap();
        assert_eq!(target.address(), "source:");
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
