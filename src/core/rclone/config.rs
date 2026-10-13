//! Generates the rclone config file pigeon passes via `--config` to every
//! invocation (ADR-0136) -- the *only* config file `rclone` is ever given.
//! Never reads an existing file, never consults `RCLONE_CONFIG` or rclone's
//! own default config-file path: every rclone-backed job is sandboxed to
//! exactly the buckets configured in pigeon's own keyring, nothing else --
//! not a separately-provisioned deployment `rclone.conf`, not the user's own
//! general-purpose one.
//!
//! Each configured bucket becomes *two* sections (ADR-0140): an `s3` backend
//! plus an `alias` section pointing into it at the configured bucket. That
//! indirection is the only place `BucketConfig::bucket` can live -- an `s3`
//! section has no bucket field of its own.

use std::path::{Path, PathBuf};

use crate::commands::keyring::store::Store;

const RCLONE_CONFIG_FILE_NAME: &str = "rclone.conf";

/// Suffix appended to a bucket-config's alias to name its `s3` backend
/// section, the inner half of ADR-0140's two-section shape. Only the outer
/// `alias` section is ever addressed by a job; this name exists purely to be
/// pointed at.
const BACKEND_SECTION_SUFFIX: &str = ".s3";

/// The `s3` backend section name for `alias`. Not public: a caller addresses
/// a bucket by its alias alone, exactly as before ADR-0140 -- that the
/// address now resolves through one more hop is entirely internal to the
/// generated file.
fn backend_section_name(alias: &str) -> String {
    format!("{alias}{BACKEND_SECTION_SUFFIX}")
}

/// Sibling to `Store::resolve_path()`'s `keyring.toml` -- same config
/// directory, so the generated file is one well-known, inspectable artifact
/// rather than a new one per run.
pub(crate) fn rclone_config_path() -> Result<PathBuf, String> {
    rclone_config_path_beside(&Store::resolve_path()?)
}

/// `rclone_config_path`'s derivation, as a pure function of the keyring's
/// location. Split out so the sibling relationship stays testable now that
/// `Store::resolve_path` can be redirected by `--keyring-path`, which lives
/// in a `OnceLock` a test cannot set per-case (ADR-0145 §3).
///
/// The generated config deliberately follows the keyring rather than
/// staying at the OS config directory: both files carry the same plaintext
/// credentials, and keeping them together means two runs against different
/// keyrings no longer race on one shared generated file -- narrowing the
/// edge case ADR-0136's Consequences named.
fn rclone_config_path_beside(keyring_path: &Path) -> Result<PathBuf, String> {
    let dir = keyring_path.parent().ok_or_else(|| {
        format!(
            "could not determine a parent directory for {}",
            keyring_path.display()
        )
    })?;
    Ok(dir.join(RCLONE_CONFIG_FILE_NAME))
}

/// Writes two sections per configured `BucketConfig` to `path`: an `s3`
/// backend named `<alias>.s3`, plus an `alias` section keyed by the
/// bucket-config's own alias whose `remote` is that backend scoped to the
/// configured bucket (`<alias>.s3:<bucket>`).
///
/// The indirection is what carries `BucketConfig::bucket` into the config at
/// all (ADR-0140). ADR-0136 wrote one `s3` section keyed directly by alias,
/// rejecting the two-section `alias -> bucket-name` shape the deployment's
/// own terraform-managed `rclone.conf` uses on the grounds that globally
/// unique aliases (`Store::contains_alias`, ADR-0022) need no collision
/// handling -- which answered a different question than where the bucket
/// name lives. An `s3` section has none, so the bucket was simply dropped:
/// `alias:` addressed the *account* root (a bucket listing) and
/// `alias:prefix/` a bucket literally named `prefix`, which is why a
/// `transform` run against `source:jpg/` matched nothing at all.
///
/// The backend is keyed by alias, not by bucket name (the terraform config's
/// own convention): aliases are unique by construction, whereas two
/// bucket-configs may legitimately name the same bucket on different
/// endpoints or with different credentials. A configured alias that is
/// literally another alias's derived backend name is rejected outright
/// rather than silently misrouted.
///
/// Regenerated fresh from the current keyring state every time this is
/// called (no existing file at `path` is ever read first), so a `pigeon
/// keyring modify bucket` takes effect on the very next job run with no
/// cache to invalidate.
pub(crate) fn write_sandboxed_config(store: &Store, path: &Path) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|err| format!("failed to create {}: {err}", parent.display()))?;
    }

    let mut contents = String::new();
    for bucket_config in store.bucket_configs() {
        let backend = backend_section_name(&bucket_config.alias);
        if store.contains_alias(&backend) {
            return Err(format!(
                "cannot generate an rclone config: the backend section for bucket-config '{}' \
                 would be named '{backend}', which collides with another configured alias -- \
                 rename one of them",
                bucket_config.alias,
            ));
        }
        contents.push_str(&format!(
            "[{backend}]\ntype = s3\nprovider = Other\naccess_key_id = {}\nsecret_access_key = {}\nendpoint = {}\nacl = private\nno_check_bucket = true\n\n[{}]\ntype = alias\nremote = {backend}:{}\n\n",
            bucket_config.access_key_id,
            bucket_config.secret_key,
            bucket_config.endpoint,
            bucket_config.alias,
            bucket_config.bucket,
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

    #[test]
    fn rclone_config_path_lands_beside_an_overridden_keyring_path() {
        let path = rclone_config_path_beside(Path::new("/etc/pigeon/keyring.toml")).unwrap();
        assert_eq!(path, PathBuf::from("/etc/pigeon/rclone.conf"));
    }

    #[test]
    fn rclone_config_path_follows_the_keyring_into_any_directory() {
        let path = rclone_config_path_beside(Path::new("/tmp/alt/custom-name.toml")).unwrap();
        assert_eq!(path, PathBuf::from("/tmp/alt/rclone.conf"));
    }

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

    /// ADR-0140: the alias section must resolve *into* the configured
    /// bucket. Without the indirection, `backup:` was the account root and
    /// `backup:jpg/` a bucket literally named `jpg`.
    #[test]
    fn write_sandboxed_config_writes_an_s3_backend_plus_a_bucket_scoped_alias_per_bucket_config() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rclone.conf");
        let mut store = Store::default();
        store.push(Entry::Bucket(sample_bucket("backup")));
        write_sandboxed_config(&store, &path).unwrap();
        let contents = std::fs::read_to_string(&path).unwrap();
        assert_eq!(
            contents,
            "[backup.s3]\n\
             type = s3\n\
             provider = Other\n\
             access_key_id = AKID\n\
             secret_access_key = shh\n\
             endpoint = https://nyc3.digitaloceanspaces.com\n\
             acl = private\n\
             no_check_bucket = true\n\
             \n\
             [backup]\n\
             type = alias\n\
             remote = backup.s3:my-bucket\n\
             \n"
        );
    }

    /// The backend is keyed by alias, not by bucket name: two bucket-configs
    /// may legitimately name the same bucket on different endpoints, and
    /// collapsing them onto one bucket-named backend section would give one
    /// of them the other's credentials.
    #[test]
    fn two_aliases_naming_the_same_bucket_get_their_own_backend_sections() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rclone.conf");
        let mut store = Store::default();
        store.push(Entry::Bucket(sample_bucket("here")));
        let mut elsewhere = sample_bucket("there");
        elsewhere.endpoint = "https://s3.fr-par.scw.cloud".to_string();
        elsewhere.access_key_id = "OTHER".to_string();
        store.push(Entry::Bucket(elsewhere));

        write_sandboxed_config(&store, &path).unwrap();
        let contents = std::fs::read_to_string(&path).unwrap();

        assert!(contents.contains("[here.s3]"));
        assert!(contents.contains("remote = here.s3:my-bucket"));
        assert!(contents.contains("[there.s3]"));
        assert!(contents.contains("remote = there.s3:my-bucket"));
        assert!(contents.contains("access_key_id = AKID"));
        assert!(contents.contains("access_key_id = OTHER"));
    }

    /// Aliases are unbounded strings, so a configured alias can collide
    /// with another's derived backend name. Rejected outright rather than
    /// silently misrouting one alias into the other's bucket.
    #[test]
    fn write_sandboxed_config_rejects_an_alias_colliding_with_a_derived_backend_name() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rclone.conf");
        let mut store = Store::default();
        store.push(Entry::Bucket(sample_bucket("backup")));
        store.push(Entry::Bucket(sample_bucket("backup.s3")));

        let err = write_sandboxed_config(&store, &path).unwrap_err();
        assert!(err.contains("backup.s3"), "unexpected error: {err}");
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
