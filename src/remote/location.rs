use std::path::PathBuf;

use crate::remote::store::Store;

/// Either side of a `copy` (or the argument to `ls`/`lsd`): a local
/// filesystem path, or a path within a configured remote's bucket.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Location {
    Local(PathBuf),
    Remote { alias: String, path: String },
}

/// Parses a `SOURCE`/`DEST`/location argument. `alias:path` is a remote
/// reference only when `alias` matches a configured remote; everything else
/// (a bare path with no colon, or `word:...` where `word` isn't a known
/// remote) is treated as a local filesystem path.
pub(crate) fn parse(arg: &str, store: &Store) -> Location {
    if let Some((alias, path)) = arg.split_once(':')
        && store.contains_alias(alias)
    {
        return Location::Remote {
            alias: alias.to_string(),
            path: path.to_string(),
        };
    }
    Location::Local(PathBuf::from(arg))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::remote::store::Remote;

    fn store_with_remote(alias: &str) -> Store {
        let mut store = Store::default();
        store.push(Remote {
            alias: alias.to_string(),
            endpoint: "https://nyc3.digitaloceanspaces.com".to_string(),
            bucket: "my-bucket".to_string(),
            access_key_id: "AKID".to_string(),
        });
        store
    }

    #[test]
    fn bare_path_is_local() {
        let store = Store::default();
        assert_eq!(
            parse("./output", &store),
            Location::Local(PathBuf::from("./output"))
        );
    }

    #[test]
    fn known_remote_with_no_path_is_remote_root() {
        let store = store_with_remote("email");
        assert_eq!(
            parse("email:", &store),
            Location::Remote {
                alias: "email".to_string(),
                path: String::new(),
            }
        );
    }

    #[test]
    fn known_remote_with_path_is_remote() {
        let store = store_with_remote("email");
        assert_eq!(
            parse("email:archive/2020", &store),
            Location::Remote {
                alias: "email".to_string(),
                path: "archive/2020".to_string(),
            }
        );
    }

    #[test]
    fn unknown_alias_with_colon_falls_back_to_local() {
        let store = store_with_remote("email");
        assert_eq!(
            parse("typo:archive", &store),
            Location::Local(PathBuf::from("typo:archive"))
        );
    }
}
