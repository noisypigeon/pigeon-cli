//! Recursive bucket listing + extension classification (ADR-0096 §2) -- no
//! downloads happen here, only `list_objects`. Own copy of
//! `deduplicate::manifest`'s checkpoint pattern (that module is private,
//! unreachable from this sibling job), reusing the hoisted
//! `core::data::extension_of`.

use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::path::Path;

use crate::commands::keyring::bucket::client;
use crate::commands::keyring::bucket::store::BucketConfig;
use crate::core::data::extension_of;

use super::classify::{self, ContentValue};

/// One source object pending processing -- `Job::gather`'s result. Only
/// objects whose extension classified `Valuable` ever become a task; a
/// `Reproducible` extension's objects are never even added to this list,
/// so they're never downloaded.
pub(crate) struct ReduceTask {
    pub key: String,
    pub size: u64,
}

/// Per-extension rollup for the wizard's pre-run verify table -- covers
/// *every* pending extension found, forwarded or not, so "which
/// directories are being forwarded" can be checked before anything
/// uploads.
#[derive(Debug, Clone)]
pub(crate) struct ExtensionSummary {
    pub extension: String,
    pub count: usize,
    pub total_bytes: u64,
    pub value: ContentValue,
}

pub(crate) struct ReducePlan {
    pub tasks: Vec<ReduceTask>,
    pub extension_summary: Vec<ExtensionSummary>,
}

/// A source object key is durably marked done here once `run()` has placed
/// it (or decided to skip it as part of a `Reproducible` extension). Same
/// role as `deduplicate::manifest::PROCESSED_FILE_NAME`.
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
/// paginated by `client::list_objects`), classifies each pending key's
/// extension (via `classify::classify_extension`, applying this run's
/// `force_valuable`/`force_reproducible` overrides), and returns a task
/// only for a key whose extension classified `Valuable` -- a `Reproducible`
/// extension's keys are counted in `extension_summary` but never enqueued,
/// so they're never downloaded. Excludes anything the `staging_dir`
/// checkpoint already marks done, mirroring `deduplicate::manifest::gather_pending`.
pub(crate) async fn gather_pending(
    bucket_config: &BucketConfig,
    secret: &str,
    staging_dir: &Path,
    force_valuable: &[String],
    force_reproducible: &[String],
) -> Result<ReducePlan, String> {
    let done = load_checkpoint(staging_dir)?;
    let entries = client::list_objects(bucket_config, secret, "", true).await?;

    let mut tasks = Vec::new();
    let mut by_extension: BTreeMap<String, (usize, u64, ContentValue)> = BTreeMap::new();

    for entry in entries {
        if entry.is_prefix || done.contains(&entry.key) {
            continue;
        }
        let extension = extension_of(&entry.key);
        let value = classify::classify_extension(&extension, force_valuable, force_reproducible);

        let bucket = by_extension
            .entry(extension.clone())
            .or_insert((0, 0, value));
        bucket.0 += 1;
        bucket.1 += entry.size;

        if value == ContentValue::Valuable {
            tasks.push(ReduceTask {
                key: entry.key,
                size: entry.size,
            });
        }
    }

    let extension_summary = by_extension
        .into_iter()
        .map(
            |(extension, (count, total_bytes, value))| ExtensionSummary {
                extension,
                count,
                total_bytes,
                value,
            },
        )
        .collect();

    Ok(ReducePlan {
        tasks,
        extension_summary,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

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
        append_checkpoint(&staging_dir, "b.mp4").unwrap();

        let loaded = load_checkpoint(&staging_dir).unwrap();
        assert!(loaded.contains("a.jpg"));
        assert!(loaded.contains("b.mp4"));
        assert_eq!(loaded.len(), 2);
    }
}
