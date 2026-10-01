//! Recursive bucket listing + extension-based type summary (ADR-0083 §3) --
//! no downloads happen here, only `list_objects`. Own copy of
//! `dedupe::manifest`'s pattern (third independent copy of this shape --
//! see ADR-0083's Context for why it isn't hoisted), with the same
//! `staging_dir`-scoped checkpoint convention ADR-0082 established.

use std::collections::HashSet;
use std::fs;
use std::path::Path;

use crate::commands::keyring::bucket::client;
use crate::commands::keyring::bucket::store::BucketConfig;

/// One source object pending processing -- `Job::gather`'s result.
pub(crate) struct SortTask {
    pub key: String,
    pub size: u64,
}

/// Per-extension rollup for the wizard's pre-run summary table.
#[derive(Debug, Clone)]
pub(crate) struct TypeSummary {
    pub extension: String,
    pub count: usize,
    pub total_bytes: u64,
}

pub(crate) struct SortPlan {
    pub tasks: Vec<SortTask>,
    pub type_summary: Vec<TypeSummary>,
}

/// A source object key is durably marked done here once it's been
/// downloaded and placed. Scoped to `staging_dir`, not `local_output`
/// directly -- keeps `.processed` out of the uploaded `result/` tree
/// (`core::data::collect_files` doesn't skip dotfiles/dot-directories).
pub(crate) const PROCESSED_FILE_NAME: &str = ".processed";

pub(crate) fn load_checkpoint(staging_dir: &Path) -> Result<HashSet<String>, String> {
    let path = staging_dir.join(PROCESSED_FILE_NAME);
    let contents = match fs::read_to_string(&path) {
        Ok(contents) => contents,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(HashSet::new()),
        Err(err) => return Err(format!("failed to read {}: {err}", path.display())),
    };
    Ok(contents.lines().map(str::to_string).collect())
}

pub(crate) fn append_checkpoint(staging_dir: &Path, key: &str) -> Result<(), String> {
    use std::io::Write;
    fs::create_dir_all(staging_dir)
        .map_err(|err| format!("failed to create {}: {err}", staging_dir.display()))?;
    let path = staging_dir.join(PROCESSED_FILE_NAME);
    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .map_err(|err| format!("failed to open {}: {err}", path.display()))?;
    writeln!(file, "{key}").map_err(|err| format!("failed to write {}: {err}", path.display()))
}

/// Lists every object in `bucket_config`'s bucket (recursive, already
/// paginated by `client::list_objects`), classifies each by its key's
/// extension for the summary table, and excludes anything the
/// `staging_dir` checkpoint already marks done.
pub(crate) async fn gather_pending(
    bucket_config: &BucketConfig,
    secret: &str,
    staging_dir: &Path,
) -> Result<SortPlan, String> {
    let done = load_checkpoint(staging_dir)?;
    let entries = client::list_objects(bucket_config, secret, "", true).await?;

    let mut tasks = Vec::with_capacity(entries.len());
    let mut by_extension: std::collections::BTreeMap<String, (usize, u64)> =
        std::collections::BTreeMap::new();

    for entry in entries {
        if entry.is_prefix || done.contains(&entry.key) {
            continue;
        }
        let extension = extension_of(&entry.key);
        let bucket = by_extension.entry(extension).or_insert((0, 0));
        bucket.0 += 1;
        bucket.1 += entry.size;
        tasks.push(SortTask {
            key: entry.key,
            size: entry.size,
        });
    }

    let type_summary = by_extension
        .into_iter()
        .map(|(extension, (count, total_bytes))| TypeSummary {
            extension,
            count,
            total_bytes,
        })
        .collect();

    Ok(SortPlan {
        tasks,
        type_summary,
    })
}

/// The lowercased extension of `key`'s final path segment, or `"(none)"`
/// when there isn't one. No canonicalization -- `.jpg` and `.jpeg` are
/// different extensions here (ADR-0083).
pub(crate) fn extension_of(key: &str) -> String {
    std::path::Path::new(key)
        .extension()
        .and_then(|ext| ext.to_str())
        .map(|ext| ext.to_ascii_lowercase())
        .unwrap_or_else(|| "(none)".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extension_of_lowercases_and_strips_the_dot() {
        assert_eq!(extension_of("Photos/IMG_0001.JPG"), "jpg");
    }

    #[test]
    fn extension_of_does_not_canonicalize_jpeg_to_jpg() {
        assert_eq!(extension_of("Photos/IMG_0002.jpeg"), "jpeg");
    }

    #[test]
    fn extension_of_handles_no_extension() {
        assert_eq!(extension_of("Photos/README"), "(none)");
    }

    #[test]
    fn extension_of_handles_dotfiles_without_extension() {
        assert_eq!(extension_of(".DS_Store"), "(none)");
    }

    #[test]
    fn load_checkpoint_is_empty_for_a_missing_staging_dir() {
        let dir = tempfile::tempdir().unwrap();
        let staging_dir = dir.path().join("does-not-exist-yet");
        assert!(load_checkpoint(&staging_dir).unwrap().is_empty());
    }

    #[test]
    fn append_checkpoint_then_load_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let staging_dir = dir.path().join(".staging");
        append_checkpoint(&staging_dir, "a.jpg").unwrap();
        append_checkpoint(&staging_dir, "b.pdf").unwrap();

        let loaded = load_checkpoint(&staging_dir).unwrap();
        assert!(loaded.contains("a.jpg"));
        assert!(loaded.contains("b.pdf"));
        assert_eq!(loaded.len(), 2);
    }
}
