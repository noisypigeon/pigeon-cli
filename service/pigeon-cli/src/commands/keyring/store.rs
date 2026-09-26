use std::path::{Path, PathBuf};

use dialoguer::{Select, theme::ColorfulTheme};
use serde::{Deserialize, Serialize};

use crate::commands::keyring::bucket::store::BucketConfig;
use crate::commands::keyring::email::identity::Identity;
use crate::commands::keyring::encryption_key::EncryptionKey;
use crate::core::keyring::KeyringEntry as _;

/// Both `email::identity`'s and `dataops::store`'s former stores read this
/// same env var name for the same reason: an override for isolating tests
/// (and manual use) from the real per-OS config directory. Unified here
/// now that there's one store consuming it (ADR-0022).
pub const CONFIG_DIR_ENV_VAR: &str = "PIGEON_CONFIG_DIR";

const KEYRING_FILE_NAME: &str = "keyring.toml";

/// One configured secret: either an authenticated email identity or a
/// bucket-config, tagged so both can live in one `keyring.toml` (ADR-0022).
/// Wraps the existing `Identity`/`BucketConfig` structs unchanged rather
/// than flattening their fields, so every consumer that already takes
/// `&Identity`/`&BucketConfig` (`imap_client::verify_login`,
/// `bucket::client::*`) needs no changes. Serde's internally-tagged
/// representation flattens the wrapped struct's own fields alongside the
/// `kind` discriminant, so `keyring.toml` still reads as a flat table per
/// entry. Named `Entry` (not `KeyringEntry`) to avoid colliding with the
/// `core::keyring::KeyringEntry` trait it implements (ADR-0023).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum Entry {
    Email(Identity),
    Bucket(BucketConfig),
    EncryptionKey(EncryptionKey),
}

impl crate::core::keyring::KeyringEntry for Entry {
    fn alias(&self) -> &str {
        match self {
            Entry::Email(identity) => identity.alias(),
            Entry::Bucket(bucket_config) => bucket_config.alias(),
            Entry::EncryptionKey(key) => key.alias(),
        }
    }

    fn kind(&self) -> &'static str {
        match self {
            Entry::Email(identity) => identity.kind(),
            Entry::Bucket(bucket_config) => bucket_config.kind(),
            Entry::EncryptionKey(key) => key.kind(),
        }
    }

    fn detail(&self) -> String {
        match self {
            Entry::Email(identity) => identity.detail(),
            Entry::Bucket(bucket_config) => bucket_config.detail(),
            Entry::EncryptionKey(key) => key.detail(),
        }
    }
}

impl Entry {
    /// A `[kind] alias - detail` label for `prompt_select`'s mixed listing.
    /// `detail()` already self-parenthesizes its own content (e.g.
    /// `willow@example.com (gmail)`), so this doesn't wrap it in another
    /// layer of parens.
    fn label(&self) -> String {
        format!("[{}] {} - {}", self.kind(), self.alias(), self.detail())
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct StoreFile {
    #[serde(default)]
    entries: Vec<Entry>,
}

/// The on-disk collection of every configured secret's metadata -- email
/// identities and bucket-configs together (ADR-0022). Replaces
/// `email::identity::Store` and `dataops::store::Store`.
#[derive(Debug, Default)]
pub struct Store {
    entries: Vec<Entry>,
}

impl Store {
    /// The keyring metadata file's path: `$PIGEON_CONFIG_DIR/keyring.toml`
    /// if set, otherwise the OS-conventional config directory for `pigeon`.
    pub fn default_path() -> Result<PathBuf, String> {
        if let Ok(dir) = std::env::var(CONFIG_DIR_ENV_VAR) {
            return Ok(PathBuf::from(dir).join(KEYRING_FILE_NAME));
        }
        let project_dirs = directories::ProjectDirs::from("", "", "pigeon")
            .ok_or("could not determine the config directory for this platform")?;
        Ok(project_dirs.config_dir().join(KEYRING_FILE_NAME))
    }

    /// Loads the store from `path`. A missing file is treated as an empty store.
    pub fn load(path: &Path) -> Result<Store, String> {
        let contents = match std::fs::read_to_string(path) {
            Ok(contents) => contents,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Store::default());
            }
            Err(err) => return Err(format!("failed to read {}: {err}", path.display())),
        };
        let file: StoreFile = toml::from_str(&contents)
            .map_err(|err| format!("failed to parse {}: {err}", path.display()))?;
        Ok(Store {
            entries: file.entries,
        })
    }

    /// Writes the store to `path`, creating its parent directory if needed.
    pub fn save(&self, path: &Path) -> Result<(), String> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|err| format!("failed to create {}: {err}", parent.display()))?;
        }
        let file = StoreFile {
            entries: self.entries.clone(),
        };
        let contents = toml::to_string_pretty(&file)
            .map_err(|err| format!("failed to serialize keyring entries: {err}"))?;
        std::fs::write(path, contents)
            .map_err(|err| format!("failed to write {}: {err}", path.display()))
    }

    /// True if `alias` is already used by *either* kind of entry -- this is
    /// what makes aliases globally unique across email identities and
    /// bucket-configs (ADR-0022).
    pub fn contains_alias(&self, alias: &str) -> bool {
        self.entries.iter().any(|entry| entry.alias() == alias)
    }

    pub fn push(&mut self, entry: Entry) {
        self.entries.push(entry);
    }

    pub fn find(&self, alias: &str) -> Option<&Entry> {
        self.entries.iter().find(|entry| entry.alias() == alias)
    }

    /// Removes the entry named `alias`, if any (of either kind). Returns
    /// whether an entry was actually removed.
    pub fn remove(&mut self, alias: &str) -> bool {
        let before = self.entries.len();
        self.entries.retain(|entry| entry.alias() != alias);
        self.entries.len() != before
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn iter(&self) -> impl Iterator<Item = &Entry> {
        self.entries.iter()
    }

    /// Every configured email identity, ignoring the other kinds -- for
    /// `job::wizard`'s identity resolution.
    pub fn email_identities(&self) -> impl Iterator<Item = &Identity> {
        self.entries.iter().filter_map(|entry| match entry {
            Entry::Email(identity) => Some(identity),
            Entry::Bucket(_) | Entry::EncryptionKey(_) => None,
        })
    }

    /// Every configured bucket-config, ignoring the other kinds -- for
    /// `job::email_sync`'s upload-target lookup.
    pub fn bucket_configs(&self) -> impl Iterator<Item = &BucketConfig> {
        self.entries.iter().filter_map(|entry| match entry {
            Entry::Bucket(bucket_config) => Some(bucket_config),
            Entry::Email(_) | Entry::EncryptionKey(_) => None,
        })
    }

    /// Every configured encryption key, ignoring the other kinds -- for
    /// `job::email_sync`'s `--encryption-key` resolution.
    pub fn encryption_keys(&self) -> impl Iterator<Item = &EncryptionKey> {
        self.entries.iter().filter_map(|entry| match entry {
            Entry::EncryptionKey(key) => Some(key),
            Entry::Email(_) | Entry::Bucket(_) => None,
        })
    }

    /// Interactively resolves which entry (of either kind) to act on: zero
    /// is an error pointing at `keyring add`; exactly one is returned
    /// without prompting; otherwise `dialoguer::Select` lists them all,
    /// labeled by kind. Used by `keyring modify` when no alias is given.
    pub fn prompt_select(&self) -> Result<&Entry, String> {
        match self.entries.as_slice() {
            [] => Err("no keyring entries configured; run 'pigeon keyring add' first".to_string()),
            [only] => Ok(only),
            entries => {
                let labels: Vec<String> = entries.iter().map(Entry::label).collect();
                let selection = Select::with_theme(&ColorfulTheme::default())
                    .with_prompt("Select an entry")
                    .items(&labels)
                    .default(0)
                    .interact()
                    .map_err(|err| format!("failed to read selection: {err}"))?;
                Ok(&entries[selection])
            }
        }
    }

    /// Same as `prompt_select`, scoped to bucket-configs only -- used by
    /// `job::wizard::resolve_remote_output` for its upload-target picker,
    /// which should never offer an email identity as a choice.
    pub fn prompt_select_bucket(&self) -> Result<&BucketConfig, String> {
        let bucket_configs: Vec<&BucketConfig> = self.bucket_configs().collect();
        match bucket_configs.as_slice() {
            [] => Err(
                "no bucket-configs configured; run 'pigeon keyring add bucket' first".to_string(),
            ),
            [only] => Ok(only),
            bucket_configs => {
                let labels: Vec<String> = bucket_configs
                    .iter()
                    .map(|bucket_config| {
                        format!(
                            "{} ({}, {})",
                            bucket_config.alias, bucket_config.endpoint, bucket_config.bucket
                        )
                    })
                    .collect();
                let selection = Select::with_theme(&ColorfulTheme::default())
                    .with_prompt("Select a bucket-config")
                    .items(&labels)
                    .default(0)
                    .interact()
                    .map_err(|err| format!("failed to read bucket-config selection: {err}"))?;
                Ok(bucket_configs[selection])
            }
        }
    }

    /// Same as `prompt_select_bucket`, scoped to encryption keys only --
    /// used by `job::email_sync::wizard::EncryptionKeyInput` when the
    /// target bucket-config requires one.
    pub fn prompt_select_encryption_key(&self) -> Result<&EncryptionKey, String> {
        let keys: Vec<&EncryptionKey> = self.encryption_keys().collect();
        match keys.as_slice() {
            [] => Err(
                "no encryption-keys configured; run 'pigeon keyring add encryption-key' first"
                    .to_string(),
            ),
            [only] => Ok(only),
            keys => {
                let labels: Vec<String> = keys
                    .iter()
                    .map(|key| format!("{} (created {})", key.alias, key.created_at))
                    .collect();
                let selection = Select::with_theme(&ColorfulTheme::default())
                    .with_prompt("Select an encryption key")
                    .items(&labels)
                    .default(0)
                    .interact()
                    .map_err(|err| format!("failed to read encryption-key selection: {err}"))?;
                Ok(keys[selection])
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::keyring::email::provider::Provider;

    fn sample_identity(alias: &str) -> Identity {
        Identity {
            alias: alias.to_string(),
            email: format!("{alias}@example.com"),
            provider: Provider::Gmail,
            host: "imap.gmail.com".to_string(),
            port: 993,
        }
    }

    fn sample_bucket_config(alias: &str) -> BucketConfig {
        BucketConfig {
            alias: alias.to_string(),
            endpoint: "https://nyc3.digitaloceanspaces.com".to_string(),
            bucket: "my-bucket".to_string(),
            access_key_id: "AKID".to_string(),
            encryption_key_alias: None,
        }
    }

    fn sample_encryption_key(alias: &str) -> EncryptionKey {
        EncryptionKey {
            alias: alias.to_string(),
            created_at: "2026-09-24T00:00:00Z".to_string(),
        }
    }

    #[test]
    fn round_trips_mixed_entries_through_toml() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("keyring.toml");

        let mut store = Store::default();
        store.push(Entry::Email(sample_identity("willow")));
        store.push(Entry::Bucket(sample_bucket_config("backup")));
        store.push(Entry::EncryptionKey(sample_encryption_key("primary")));
        store.save(&path).unwrap();

        let loaded = Store::load(&path).unwrap();
        assert_eq!(loaded.iter().count(), 3);
        assert!(loaded.contains_alias("willow"));
        assert!(loaded.contains_alias("backup"));
        assert!(loaded.contains_alias("primary"));
        assert_eq!(loaded.email_identities().count(), 1);
        assert_eq!(loaded.bucket_configs().count(), 1);
        assert_eq!(loaded.encryption_keys().count(), 1);
    }

    #[test]
    fn bucket_config_encryption_key_alias_round_trips_through_toml() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("keyring.toml");

        let mut bucket = sample_bucket_config("backup");
        bucket.encryption_key_alias = Some("primary".to_string());

        let mut store = Store::default();
        store.push(Entry::Bucket(bucket));
        store.save(&path).unwrap();

        let loaded = Store::load(&path).unwrap();
        let loaded_bucket = loaded.bucket_configs().next().unwrap();
        assert_eq!(
            loaded_bucket.encryption_key_alias,
            Some("primary".to_string())
        );
        assert_eq!(
            loaded_bucket.detail(),
            "https://nyc3.digitaloceanspaces.com (my-bucket), encrypts with 'primary'"
        );
    }

    #[test]
    fn missing_file_is_empty_store() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("does-not-exist.toml");

        let loaded = Store::load(&path).unwrap();
        assert!(loaded.is_empty());
    }

    #[test]
    fn contains_alias_is_global_across_kinds() {
        let mut store = Store::default();
        store.push(Entry::Bucket(sample_bucket_config("shared-alias")));

        // An email identity trying to reuse a bucket-config's alias is
        // caught by the same check -- this is what makes aliases globally
        // unique (ADR-0022).
        assert!(store.contains_alias("shared-alias"));
    }

    #[test]
    fn email_identities_ignores_bucket_entries() {
        let mut store = Store::default();
        store.push(Entry::Email(sample_identity("willow")));
        store.push(Entry::Bucket(sample_bucket_config("backup")));

        let aliases: Vec<&str> = store.email_identities().map(|i| i.alias.as_str()).collect();
        assert_eq!(aliases, vec!["willow"]);
    }

    #[test]
    fn bucket_configs_ignores_email_entries() {
        let mut store = Store::default();
        store.push(Entry::Email(sample_identity("willow")));
        store.push(Entry::Bucket(sample_bucket_config("backup")));

        let aliases: Vec<&str> = store.bucket_configs().map(|b| b.alias.as_str()).collect();
        assert_eq!(aliases, vec!["backup"]);
    }

    #[test]
    fn encryption_keys_ignores_other_kinds() {
        let mut store = Store::default();
        store.push(Entry::Email(sample_identity("willow")));
        store.push(Entry::Bucket(sample_bucket_config("backup")));
        store.push(Entry::EncryptionKey(sample_encryption_key("primary")));

        let aliases: Vec<&str> = store.encryption_keys().map(|k| k.alias.as_str()).collect();
        assert_eq!(aliases, vec!["primary"]);
    }

    #[test]
    fn find_returns_none_for_unknown_alias() {
        let store = Store::default();
        assert!(store.find("nope").is_none());
    }

    #[test]
    fn remove_deletes_matching_entry_of_either_kind() {
        let mut store = Store::default();
        store.push(Entry::Email(sample_identity("willow")));
        store.push(Entry::Bucket(sample_bucket_config("backup")));

        assert!(store.remove("willow"));
        assert_eq!(store.iter().count(), 1);
        assert!(store.remove("backup"));
        assert!(store.is_empty());
        assert!(!store.remove("backup"));
    }

    #[test]
    fn label_formats_both_kinds_distinctly() {
        let email = Entry::Email(sample_identity("willow"));
        let bucket = Entry::Bucket(sample_bucket_config("backup"));
        assert_eq!(email.label(), "[email] willow - willow@example.com (gmail)");
        assert_eq!(
            bucket.label(),
            "[bucket] backup - https://nyc3.digitaloceanspaces.com (my-bucket)"
        );
    }

    #[test]
    fn entry_detail_and_kind_delegate_to_the_wrapped_variant() {
        let email = Entry::Email(sample_identity("willow"));
        let bucket = Entry::Bucket(sample_bucket_config("backup"));
        assert_eq!(email.kind(), "email");
        assert_eq!(email.detail(), "willow@example.com (gmail)");
        assert_eq!(bucket.kind(), "bucket");
        assert_eq!(
            bucket.detail(),
            "https://nyc3.digitaloceanspaces.com (my-bucket)"
        );
    }

    #[test]
    fn prompt_select_errors_on_empty_store() {
        let store = Store::default();
        assert!(store.prompt_select().is_err());
    }

    #[test]
    fn prompt_select_auto_selects_the_only_entry() {
        let mut store = Store::default();
        store.push(Entry::Email(sample_identity("willow")));
        assert_eq!(store.prompt_select().unwrap().alias(), "willow");
    }

    #[test]
    fn prompt_select_bucket_ignores_email_entries_when_auto_selecting() {
        let mut store = Store::default();
        store.push(Entry::Email(sample_identity("willow")));
        store.push(Entry::Bucket(sample_bucket_config("backup")));
        assert_eq!(store.prompt_select_bucket().unwrap().alias, "backup");
    }

    #[test]
    fn prompt_select_bucket_errors_when_none_configured() {
        let mut store = Store::default();
        store.push(Entry::Email(sample_identity("willow")));
        assert!(store.prompt_select_bucket().is_err());
    }

    #[test]
    fn prompt_select_encryption_key_ignores_other_kinds_when_auto_selecting() {
        let mut store = Store::default();
        store.push(Entry::Email(sample_identity("willow")));
        store.push(Entry::Bucket(sample_bucket_config("backup")));
        store.push(Entry::EncryptionKey(sample_encryption_key("primary")));
        assert_eq!(
            store.prompt_select_encryption_key().unwrap().alias,
            "primary"
        );
    }

    #[test]
    fn prompt_select_encryption_key_errors_when_none_configured() {
        let mut store = Store::default();
        store.push(Entry::Email(sample_identity("willow")));
        assert!(store.prompt_select_encryption_key().is_err());
    }
}
