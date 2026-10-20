use std::path::{Path, PathBuf};

use dialoguer::{Select, theme::ColorfulTheme};
use serde::{Deserialize, Serialize};

use crate::commands::keyring::bucket::store::BucketConfig;
use crate::commands::keyring::email::identity::Identity;
use crate::core::keyring::KeyringEntry as _;

/// Both `email::identity`'s and `dataops::store`'s former stores read this
/// same env var name for the same reason: an override for isolating tests
/// (and manual use) from the real per-OS config directory. Unified here
/// now that there's one store consuming it (CLI-0022).
pub const CONFIG_DIR_ENV_VAR: &str = "PIGEON_CONFIG_DIR";

const KEYRING_FILE_NAME: &str = "keyring.toml";

/// An explicit `--keyring-path` (CLI-0145), recorded once from `main` before
/// any command dispatch and taking precedence over `CONFIG_DIR_ENV_VAR`.
///
/// A process-global rather than a parameter threaded down from the CLI
/// because `core::rclone::config::rclone_config_path` derives the generated
/// `rclone.conf`'s directory from this path deep inside
/// `core::rclone::execute`, which has no access to any wizard's arguments --
/// threading it there would mean changing `RcloneCommand::run`'s signature
/// and every `Copy`/`CopyTo`/`Lsjson`/`Purge` call site. Same reason
/// `observability`'s `LOG_FILE_PATH` is a `OnceLock` (CLI-0073).
static KEYRING_PATH_OVERRIDE: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();

/// Records `--keyring-path`. Called once from `main`, before `dispatch`.
pub fn set_keyring_path_override(path: PathBuf) {
    let _ = KEYRING_PATH_OVERRIDE.set(path);
}

/// The explicitly-passed `--keyring-path`, if there was one. Distinguishes
/// "the operator named this file" from "pigeon derived it", which is what
/// lets a job refuse a named file that doesn't exist rather than silently
/// loading an empty store (CLI-0145 §4).
pub fn keyring_path_override() -> Option<&'static Path> {
    KEYRING_PATH_OVERRIDE.get().map(PathBuf::as_path)
}

/// One configured secret: either an authenticated email identity or a
/// bucket-config, tagged so both can live in one `keyring.toml` (CLI-0022).
/// Wraps the existing `Identity`/`BucketConfig` structs unchanged rather
/// than flattening their fields, so every consumer that already takes
/// `&Identity`/`&BucketConfig` (`imap_client::verify_login`,
/// `bucket::client::*`) needs no changes. Serde's internally-tagged
/// representation flattens the wrapped struct's own fields alongside the
/// `kind` discriminant, so `keyring.toml` still reads as a flat table per
/// entry. Named `Entry` (not `KeyringEntry`) to avoid colliding with the
/// `core::keyring::KeyringEntry` trait it implements (CLI-0023).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum Entry {
    Email(Identity),
    Bucket(BucketConfig),
}

impl crate::core::keyring::KeyringEntry for Entry {
    fn alias(&self) -> &str {
        match self {
            Entry::Email(identity) => identity.alias(),
            Entry::Bucket(bucket_config) => bucket_config.alias(),
        }
    }

    fn kind(&self) -> &'static str {
        match self {
            Entry::Email(identity) => identity.kind(),
            Entry::Bucket(bucket_config) => bucket_config.kind(),
        }
    }

    fn detail(&self) -> String {
        match self {
            Entry::Email(identity) => identity.detail(),
            Entry::Bucket(bucket_config) => bucket_config.detail(),
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

/// The entry kind CLI-0147 removed along with client-side encryption.
/// Matched against a parse failure's text purely to add remediation, never
/// to change what is accepted.
const REMOVED_ENCRYPTION_KEY_KIND: &str = "encryption-key";

/// Formats a `keyring.toml` parse failure, appending remediation when the
/// cause is a leftover `kind = "encryption-key"` entry (CLI-0147 §2).
///
/// `Entry` is internally tagged, and serde rejects an *unknown variant tag*
/// regardless of `deny_unknown_fields` -- so one stale entry fails
/// `Store::load`, which every command calls, including the
/// `pigeon keyring list`/`delete` an operator would reach for to remove it.
/// Without this hint the raw error names neither the cause nor the fix,
/// leaving no in-CLI path out.
///
/// Deliberately a better *message* rather than a `#[serde(other)]`
/// catch-all variant: tolerating unknown kinds would self-heal here but
/// would also silently swallow a genuinely malformed `kind`, reversing the
/// fail-loudly posture CLI-0135 chose for the file that now holds every
/// secret in plaintext. Same reasoning as CLI-0145's `load_keyring` guard,
/// which answered a confusing failure with a clearer error, not tolerance.
fn parse_error(path: &Path, error: &str) -> String {
    let base = format!("failed to parse {}: {error}", path.display());
    if error.contains(REMOVED_ENCRYPTION_KEY_KIND) {
        return format!(
            "{base}\n\nCLI-0147 removed the '{REMOVED_ENCRYPTION_KEY_KIND}' entry kind along with \
             client-side encryption. Delete the [[entries]] block with \
             kind = \"{REMOVED_ENCRYPTION_KEY_KIND}\" from {} to continue.",
            path.display()
        );
    }
    base
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct StoreFile {
    #[serde(default)]
    entries: Vec<Entry>,
}

/// The on-disk collection of every configured secret's metadata -- email
/// identities and bucket-configs together (CLI-0022). Replaces
/// `email::identity::Store` and `dataops::store::Store`.
#[derive(Debug, Default)]
pub struct Store {
    entries: Vec<Entry>,
}

/// `Store::resolve_path`'s precedence, as a pure function of its two
/// overrides. Extracted so the ordering is unit-testable: the real
/// `--keyring-path` lives in a `OnceLock` that can only be set once per
/// process, so an in-process test could otherwise only ever exercise one
/// branch (CLI-0143's `circuit_breaker_should_trip` precedent).
fn resolve_path_from(
    override_path: Option<&Path>,
    config_dir: Option<String>,
) -> Result<PathBuf, String> {
    if let Some(path) = override_path {
        return Ok(path.to_path_buf());
    }
    if let Some(dir) = config_dir {
        return Ok(PathBuf::from(dir).join(KEYRING_FILE_NAME));
    }
    let project_dirs = directories::ProjectDirs::from("", "", "pigeon")
        .ok_or("could not determine the config directory for this platform")?;
    Ok(project_dirs.config_dir().join(KEYRING_FILE_NAME))
}

impl Store {
    /// The keyring file's path: `--keyring-path` if it was passed, otherwise
    /// `$PIGEON_CONFIG_DIR/keyring.toml` if that's set, otherwise the
    /// OS-conventional config directory for `pigeon` (CLI-0145).
    ///
    /// Named `resolve_path` rather than CLI-0022's `default_path`: with two
    /// overrides ahead of the default, the old name named only the last
    /// branch.
    pub fn resolve_path() -> Result<PathBuf, String> {
        resolve_path_from(
            keyring_path_override(),
            std::env::var(CONFIG_DIR_ENV_VAR).ok(),
        )
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
        let file: StoreFile =
            toml::from_str(&contents).map_err(|err| parse_error(path, &err.to_string()))?;
        Ok(Store {
            entries: file.entries,
        })
    }

    /// Writes the store to `path`, creating its parent directory if needed.
    /// Every entry's secret lives in this file as plaintext (CLI-0135), so
    /// on Unix this also sets `0600` permissions -- the file's only
    /// protection now that there's no OS keychain backing it.
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
            .map_err(|err| format!("failed to write {}: {err}", path.display()))?;

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
                .map_err(|err| format!("failed to set permissions on {}: {err}", path.display()))?;
        }

        Ok(())
    }

    /// True if `alias` is already used by *either* kind of entry -- this is
    /// what makes aliases globally unique across email identities and
    /// bucket-configs (CLI-0022).
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

    /// Every configured email identity, ignoring the other kind -- for
    /// `job run email import`'s identity resolution.
    pub fn email_identities(&self) -> impl Iterator<Item = &Identity> {
        self.entries.iter().filter_map(|entry| match entry {
            Entry::Email(identity) => Some(identity),
            Entry::Bucket(_) => None,
        })
    }

    /// Every configured bucket-config, ignoring the other kind -- for
    /// each job's destination and report-bucket lookup.
    pub fn bucket_configs(&self) -> impl Iterator<Item = &BucketConfig> {
        self.entries.iter().filter_map(|entry| match entry {
            Entry::Bucket(bucket_config) => Some(bucket_config),
            Entry::Email(_) => None,
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
    ///
    /// Takes no default-row hint: CLI-0137's `default_alias` parameter was
    /// removed by CLI-0138, which moved `transform`'s `--input-file-type`
    /// inference off the bucket alias and onto the address prefix, leaving
    /// nothing anywhere in the crate with an alias to pre-select.
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::keyring::email::provider::Provider;

    #[test]
    fn resolve_path_from_prefers_an_explicit_keyring_path_over_the_config_dir() {
        let path = resolve_path_from(
            Some(Path::new("/explicit/keyring.toml")),
            Some("/from/env".to_string()),
        )
        .unwrap();
        assert_eq!(path, PathBuf::from("/explicit/keyring.toml"));
    }

    #[test]
    fn resolve_path_from_falls_back_to_the_config_dir_when_no_override_is_set() {
        let path = resolve_path_from(None, Some("/from/env".to_string())).unwrap();
        assert_eq!(path, PathBuf::from("/from/env/keyring.toml"));
    }

    #[test]
    fn resolve_path_from_falls_back_to_the_os_config_dir_when_neither_is_set() {
        // Only the file name is asserted: the directory half is whatever
        // `directories` reports for this platform, which varies by OS.
        let path = resolve_path_from(None, None).unwrap();
        assert_eq!(path.file_name().unwrap(), "keyring.toml");
        assert!(path.parent().is_some());
    }

    #[test]
    fn resolve_path_from_uses_an_explicit_path_verbatim_without_appending_a_file_name() {
        // `--keyring-path` names a *file*; `PIGEON_CONFIG_DIR` names a
        // *directory*. Confirms the two are not conflated (CLI-0145 §1).
        let path = resolve_path_from(Some(Path::new("/tmp/alt.toml")), None).unwrap();
        assert_eq!(path, PathBuf::from("/tmp/alt.toml"));
    }

    fn sample_identity(alias: &str) -> Identity {
        Identity {
            alias: alias.to_string(),
            email: format!("{alias}@example.com"),
            provider: Provider::Gmail,
            host: "imap.gmail.com".to_string(),
            port: 993,
            max_imap_connections: None,
            security: None,
            secret: "app-password".to_string(),
        }
    }

    fn sample_bucket_config(alias: &str) -> BucketConfig {
        BucketConfig {
            alias: alias.to_string(),
            endpoint: "https://nyc3.digitaloceanspaces.com".to_string(),
            bucket: "my-bucket".to_string(),
            access_key_id: "AKID".to_string(),
            secret_key: "s3-secret-key".to_string(),
        }
    }

    #[test]
    fn round_trips_mixed_entries_through_toml() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("keyring.toml");

        let mut store = Store::default();
        store.push(Entry::Email(sample_identity("willow")));
        store.push(Entry::Bucket(sample_bucket_config("backup")));
        store.save(&path).unwrap();

        let loaded = Store::load(&path).unwrap();
        assert_eq!(loaded.iter().count(), 2);
        assert!(loaded.contains_alias("willow"));
        assert!(loaded.contains_alias("backup"));
        assert_eq!(loaded.email_identities().count(), 1);
        assert_eq!(loaded.bucket_configs().count(), 1);
        assert_eq!(
            loaded.email_identities().next().unwrap().secret,
            "app-password"
        );
        assert_eq!(
            loaded.bucket_configs().next().unwrap().secret_key,
            "s3-secret-key"
        );
    }

    /// CLI-0147 §2: `Entry` is internally tagged, so serde rejects an
    /// unknown variant tag outright -- and `Store::load` runs for every
    /// command, including the `keyring list`/`delete` an operator would
    /// reach for to remove the offending entry. The error therefore has to
    /// name both the cause and the fix, or there is no in-CLI path out.
    #[test]
    fn load_explains_how_to_remove_a_leftover_encryption_key_entry() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("keyring.toml");
        std::fs::write(
            &path,
            "[[entries]]\nkind = \"encryption-key\"\nalias = \"primary\"\n\
             created_at = \"2026-09-24T00:00:00Z\"\nkey_hex = \"0\"\n",
        )
        .unwrap();

        let err = Store::load(&path).unwrap_err();

        assert!(err.contains("encryption-key"), "unexpected error: {err}");
        assert!(err.contains("CLI-0147"), "unexpected error: {err}");
        assert!(
            err.contains(&path.display().to_string()),
            "the error must name the file to edit: {err}"
        );
        assert!(
            err.contains("[[entries]]"),
            "the error must name the block to delete: {err}"
        );
    }

    /// A stale `encryption_key_alias` on a *bucket* entry is a different
    /// story from a stale entry *kind*: with no `deny_unknown_fields`
    /// anywhere in the crate, serde ignores the unknown field, so this
    /// needs no migration at all.
    #[test]
    fn load_ignores_a_stale_encryption_key_alias_on_a_bucket_entry() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("keyring.toml");
        std::fs::write(
            &path,
            "[[entries]]\nkind = \"bucket\"\nalias = \"backup\"\n\
             endpoint = \"https://example.com\"\nbucket = \"b\"\n\
             access_key_id = \"AKID\"\nsecret_key = \"shh\"\n\
             encryption_key_alias = \"primary\"\n",
        )
        .unwrap();

        let loaded = Store::load(&path).unwrap();

        assert_eq!(loaded.bucket_configs().count(), 1);
        assert_eq!(loaded.bucket_configs().next().unwrap().alias, "backup");
    }

    #[test]
    fn load_rejects_a_pre_migration_file_missing_the_secret_field() {
        // Simulates a `keyring.toml` written before CLI-0135 added secret
        // fields -- unlike `max_imap_connections` (CLI-0080), these fields
        // deliberately have no `#[serde(default)]`: a missing secret must
        // fail loudly, not silently load as an empty string.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("keyring.toml");
        std::fs::write(
            &path,
            r#"[[entries]]
kind = "email"
alias = "willow"
email = "willow@example.com"
provider = "gmail"
host = "imap.gmail.com"
port = 993
"#,
        )
        .unwrap();

        assert!(Store::load(&path).is_err());
    }

    #[test]
    #[cfg(unix)]
    fn save_sets_0600_permissions_on_the_written_file() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("keyring.toml");

        let mut store = Store::default();
        store.push(Entry::Bucket(sample_bucket_config("backup")));
        store.save(&path).unwrap();

        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    #[test]
    fn email_identity_max_imap_connections_round_trips_through_toml() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("keyring.toml");

        let mut identity = sample_identity("willow");
        identity.max_imap_connections = Some(2);

        let mut store = Store::default();
        store.push(Entry::Email(identity));
        store.save(&path).unwrap();

        let loaded = Store::load(&path).unwrap();
        let loaded_identity = loaded.email_identities().next().unwrap();
        assert_eq!(loaded_identity.max_imap_connections, Some(2));
        assert_eq!(
            loaded_identity.detail(),
            "willow@example.com (gmail, max 2 IMAP conns)"
        );
    }

    #[test]
    fn email_identity_without_max_imap_connections_field_defaults_to_none() {
        // Simulates a `keyring.toml` written before CLI-0080 added this
        // field -- `#[serde(default)]` must let it parse rather than fail.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("keyring.toml");
        std::fs::write(
            &path,
            r#"[[entries]]
kind = "email"
alias = "willow"
email = "willow@example.com"
provider = "gmail"
host = "imap.gmail.com"
port = 993
secret = "app-password"
"#,
        )
        .unwrap();

        let loaded = Store::load(&path).unwrap();
        let loaded_identity = loaded.email_identities().next().unwrap();
        assert_eq!(loaded_identity.max_imap_connections, None);
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
        // unique (CLI-0022).
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
    fn prompt_select_bucket_with_two_or_more_configs_does_not_panic_without_a_tty() {
        let mut store = Store::default();
        store.push(Entry::Bucket(sample_bucket_config("heic")));
        store.push(Entry::Bucket(sample_bucket_config("jpg")));
        // No TTY under `cargo test`'s captured stdin -- this asserts a
        // clean error from `Select::interact()`, not a panic. Kept from
        // CLI-0137 (which added it alongside a default-row hint CLI-0138
        // then removed): it remains the only coverage this branch has.
        assert!(store.prompt_select_bucket().is_err());
    }
}
