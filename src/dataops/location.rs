use std::path::PathBuf;

use crate::dataops::store::Store;

/// Either side of a `copy` (or the argument to `ls`/`lsd`): a local
/// filesystem path, or a path within a configured bucket-config's bucket.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Location {
    Local(PathBuf),
    Bucket { alias: String, path: String },
}

/// Parses a `SOURCE`/`DEST`/location argument. `alias:path` is a bucket
/// reference only when `alias` matches a configured bucket-config;
/// everything else (a bare path with no colon, or `word:...` where `word`
/// isn't a known bucket-config) is treated as a local filesystem path.
pub(crate) fn parse(arg: &str, store: &Store) -> Location {
    if let Some((alias, path)) = arg.split_once(':')
        && store.contains_alias(alias)
    {
        return Location::Bucket {
            alias: alias.to_string(),
            path: path.to_string(),
        };
    }
    Location::Local(PathBuf::from(arg))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dataops::store::BucketConfig;

    fn store_with_bucket_config(alias: &str) -> Store {
        let mut store = Store::default();
        store.push(BucketConfig {
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
    fn known_bucket_config_with_no_path_is_bucket_root() {
        let store = store_with_bucket_config("email");
        assert_eq!(
            parse("email:", &store),
            Location::Bucket {
                alias: "email".to_string(),
                path: String::new(),
            }
        );
    }

    #[test]
    fn known_bucket_config_with_path_is_bucket() {
        let store = store_with_bucket_config("email");
        assert_eq!(
            parse("email:archive/2020", &store),
            Location::Bucket {
                alias: "email".to_string(),
                path: "archive/2020".to_string(),
            }
        );
    }

    #[test]
    fn unknown_alias_with_colon_falls_back_to_local() {
        let store = store_with_bucket_config("email");
        assert_eq!(
            parse("typo:archive", &store),
            Location::Local(PathBuf::from("typo:archive"))
        );
    }
}
