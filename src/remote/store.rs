use std::path::{Path, PathBuf};

use dialoguer::{Select, theme::ColorfulTheme};
use serde::{Deserialize, Serialize};

/// Same env var name as `email::identity`'s -- both stores live under one
/// `pigeon` config directory, just as separate files. Redefined locally
/// rather than imported from `email` so the two command groups stay
/// independent siblings per ADR-0008, at the cost of one duplicated line.
pub const CONFIG_DIR_ENV_VAR: &str = "PIGEON_CONFIG_DIR";

const REMOTES_FILE_NAME: &str = "remotes.toml";

/// A single configured S3-compatible remote's non-secret metadata. The
/// secret access key lives in the OS keychain, keyed by `alias` -- see
/// `crate::remote::credentials`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Remote {
    pub alias: String,
    pub endpoint: String,
    pub bucket: String,
    pub access_key_id: String,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct StoreFile {
    #[serde(default)]
    remotes: Vec<Remote>,
}

/// The on-disk collection of configured remotes' metadata.
#[derive(Debug, Default)]
pub struct Store {
    remotes: Vec<Remote>,
}

impl Store {
    /// The remote metadata file's path: `$PIGEON_CONFIG_DIR/remotes.toml`
    /// if set, otherwise the OS-conventional config directory for `pigeon`.
    pub fn default_path() -> Result<PathBuf, String> {
        if let Ok(dir) = std::env::var(CONFIG_DIR_ENV_VAR) {
            return Ok(PathBuf::from(dir).join(REMOTES_FILE_NAME));
        }
        let project_dirs = directories::ProjectDirs::from("", "", "pigeon")
            .ok_or("could not determine the config directory for this platform")?;
        Ok(project_dirs.config_dir().join(REMOTES_FILE_NAME))
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
            remotes: file.remotes,
        })
    }

    /// Writes the store to `path`, creating its parent directory if needed.
    pub fn save(&self, path: &Path) -> Result<(), String> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|err| format!("failed to create {}: {err}", parent.display()))?;
        }
        let file = StoreFile {
            remotes: self.remotes.clone(),
        };
        let contents = toml::to_string_pretty(&file)
            .map_err(|err| format!("failed to serialize remotes: {err}"))?;
        std::fs::write(path, contents)
            .map_err(|err| format!("failed to write {}: {err}", path.display()))
    }

    pub fn contains_alias(&self, alias: &str) -> bool {
        self.remotes.iter().any(|remote| remote.alias == alias)
    }

    pub fn push(&mut self, remote: Remote) {
        self.remotes.push(remote);
    }

    pub fn iter(&self) -> impl Iterator<Item = &Remote> {
        self.remotes.iter()
    }

    pub fn is_empty(&self) -> bool {
        self.remotes.is_empty()
    }

    pub fn find(&self, alias: &str) -> Option<&Remote> {
        self.remotes.iter().find(|remote| remote.alias == alias)
    }

    pub fn find_mut(&mut self, alias: &str) -> Option<&mut Remote> {
        self.remotes.iter_mut().find(|remote| remote.alias == alias)
    }

    /// Removes the remote named `alias`, if any. Returns whether an entry
    /// was actually removed.
    pub fn remove(&mut self, alias: &str) -> bool {
        let before = self.remotes.len();
        self.remotes.retain(|remote| remote.alias != alias);
        self.remotes.len() != before
    }

    /// Interactively resolves which remote to act on: zero remotes is an
    /// error pointing at `configure`; exactly one is returned without
    /// prompting; otherwise `dialoguer::Select` lists them, same pattern as
    /// `email::identity::Store::prompt_select()`.
    pub fn prompt_select(&self) -> Result<&Remote, String> {
        match self.remotes.as_slice() {
            [] => Err("no remotes configured; run 'pigeon remote configure' first".to_string()),
            [only] => Ok(only),
            remotes => {
                let labels: Vec<String> = remotes
                    .iter()
                    .map(|remote| {
                        format!("{} ({}, {})", remote.alias, remote.endpoint, remote.bucket)
                    })
                    .collect();
                let selection = Select::with_theme(&ColorfulTheme::default())
                    .with_prompt("Select a remote")
                    .items(&labels)
                    .default(0)
                    .interact()
                    .map_err(|err| format!("failed to read remote selection: {err}"))?;
                Ok(&remotes[selection])
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_remote(alias: &str) -> Remote {
        Remote {
            alias: alias.to_string(),
            endpoint: "https://nyc3.digitaloceanspaces.com".to_string(),
            bucket: "my-bucket".to_string(),
            access_key_id: "AKID".to_string(),
        }
    }

    #[test]
    fn round_trips_through_toml() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("remotes.toml");

        let mut store = Store::default();
        store.push(sample_remote("email"));
        store.save(&path).unwrap();

        let loaded = Store::load(&path).unwrap();
        assert!(loaded.contains_alias("email"));
        assert_eq!(loaded.iter().count(), 1);
        assert_eq!(loaded.find("email").unwrap().bucket, "my-bucket");
    }

    #[test]
    fn missing_file_is_empty_store() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("does-not-exist.toml");

        let loaded = Store::load(&path).unwrap();
        assert!(loaded.is_empty());
    }

    #[test]
    fn find_returns_none_for_unknown_alias() {
        let store = Store::default();
        assert!(store.find("nope").is_none());
    }

    #[test]
    fn find_mut_allows_in_place_update() {
        let mut store = Store::default();
        store.push(sample_remote("email"));

        store.find_mut("email").unwrap().bucket = "new-bucket".to_string();

        assert_eq!(store.find("email").unwrap().bucket, "new-bucket");
    }

    #[test]
    fn remove_deletes_matching_entry() {
        let mut store = Store::default();
        store.push(sample_remote("email"));

        assert!(store.remove("email"));
        assert!(store.is_empty());
        assert!(!store.remove("email"));
    }
}
