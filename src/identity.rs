use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::provider::Provider;

/// An environment variable that, when set, overrides the identity metadata
/// file's directory. Used to isolate black-box CLI tests from the real
/// per-OS config directory (and available as a manual override besides).
pub const CONFIG_DIR_ENV_VAR: &str = "PIGEON_CONFIG_DIR";

const IDENTITIES_FILE_NAME: &str = "identities.toml";

/// A single authenticated email identity's non-secret metadata. The actual
/// secret (app/bridge password) lives in the OS keychain, keyed by `alias`
/// -- see `crate::credentials`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Identity {
    pub alias: String,
    pub email: String,
    pub provider: Provider,
    pub host: String,
    pub port: u16,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct StoreFile {
    #[serde(default)]
    identities: Vec<Identity>,
}

/// The on-disk collection of authenticated identities' metadata.
#[derive(Debug, Default)]
pub struct Store {
    identities: Vec<Identity>,
}

impl Store {
    /// The identity metadata file's path: `$PIGEON_CONFIG_DIR/identities.toml`
    /// if set, otherwise the OS-conventional config directory for `pigeon`.
    pub fn default_path() -> Result<PathBuf, String> {
        if let Ok(dir) = std::env::var(CONFIG_DIR_ENV_VAR) {
            return Ok(PathBuf::from(dir).join(IDENTITIES_FILE_NAME));
        }
        let project_dirs = directories::ProjectDirs::from("", "", "pigeon")
            .ok_or("could not determine the config directory for this platform")?;
        Ok(project_dirs.config_dir().join(IDENTITIES_FILE_NAME))
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
            identities: file.identities,
        })
    }

    /// Writes the store to `path`, creating its parent directory if needed.
    pub fn save(&self, path: &Path) -> Result<(), String> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|err| format!("failed to create {}: {err}", parent.display()))?;
        }
        let file = StoreFile {
            identities: self.identities.clone(),
        };
        let contents = toml::to_string_pretty(&file)
            .map_err(|err| format!("failed to serialize identities: {err}"))?;
        std::fs::write(path, contents)
            .map_err(|err| format!("failed to write {}: {err}", path.display()))
    }

    pub fn contains_alias(&self, alias: &str) -> bool {
        self.identities
            .iter()
            .any(|identity| identity.alias == alias)
    }

    pub fn push(&mut self, identity: Identity) {
        self.identities.push(identity);
    }

    pub fn iter(&self) -> impl Iterator<Item = &Identity> {
        self.identities.iter()
    }

    pub fn is_empty(&self) -> bool {
        self.identities.is_empty()
    }
}

/// Derives a default alias from an email address's local part, following
/// ADR-0001's file-naming scheme: lowercase, non-alphanumeric runs collapsed
/// to a single hyphen, leading/trailing hyphens trimmed.
///
/// e.g. `first.last@example.com` -> `first-last`
pub fn sanitize_alias(email: &str) -> String {
    let local_part = email.split('@').next().unwrap_or(email);
    let mut alias = String::with_capacity(local_part.len());
    let mut last_was_hyphen = false;
    for ch in local_part.chars() {
        if ch.is_ascii_alphanumeric() {
            alias.push(ch.to_ascii_lowercase());
            last_was_hyphen = false;
        } else if !last_was_hyphen && !alias.is_empty() {
            alias.push('-');
            last_was_hyphen = true;
        }
    }
    if alias.ends_with('-') {
        alias.pop();
    }
    alias
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitizes_dotted_local_part() {
        assert_eq!(sanitize_alias("first.last@example.com"), "first-last");
    }

    #[test]
    fn sanitizes_plus_addressing() {
        assert_eq!(sanitize_alias("jane+work@example.com"), "jane-work");
    }

    #[test]
    fn round_trips_through_toml() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("identities.toml");

        let mut store = Store::default();
        store.push(Identity {
            alias: "first-last".to_string(),
            email: "first.last@example.com".to_string(),
            provider: Provider::Gmail,
            host: "imap.gmail.com".to_string(),
            port: 993,
        });
        store.save(&path).unwrap();

        let loaded = Store::load(&path).unwrap();
        assert!(loaded.contains_alias("first-last"));
        assert_eq!(loaded.iter().count(), 1);
    }

    #[test]
    fn missing_file_is_empty_store() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("does-not-exist.toml");

        let loaded = Store::load(&path).unwrap();
        assert!(loaded.is_empty());
    }
}
