//! Recursive bucket listing + extension-based type summary (ADR-0082 §3) --
//! no downloads happen here, only `list_objects`. Own copy of
//! `pull_transform::manifest`'s pattern (that module is private, unreachable
//! from this sibling job), with one deliberate layout correction: checkpoint
//! functions take `staging_dir`, not `local_output` itself -- keeps
//! `.processed`/`.content-hashes` out of the uploaded `result/` tree (see
//! `deduplicate/mod.rs`'s top-level doc comment).

use std::collections::HashSet;
use std::fs;
use std::path::Path;

use crate::commands::keyring::bucket::client;
use crate::commands::keyring::bucket::store::BucketConfig;
pub(crate) use crate::core::data::extension_of;

/// One source object pending processing -- `Job::gather`'s result. Only
/// top-level bucket objects; zip members discovered during `run()` are
/// queued dynamically there, never appear in this list.
pub(crate) struct DeduplicateTask {
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

pub(crate) struct DeduplicatePlan {
    pub tasks: Vec<DeduplicateTask>,
    pub type_summary: Vec<TypeSummary>,
}

/// A source object key is durably marked done here once `run()` has fully
/// handled it (itself, for a non-zip; its own successful expansion, for a
/// zip). Same role as `pull_transform::manifest::PROCESSED_FILE_NAME`, just
/// scoped to `staging_dir` instead of `local_output`.
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
/// `staging_dir` checkpoint already marks done -- both the returned task
/// list and the summary table reflect only *pending* work, mirroring
/// `pull_transform::manifest::gather_pending`.
pub(crate) async fn gather_pending(
    bucket_config: &BucketConfig,
    secret: &str,
    staging_dir: &Path,
) -> Result<DeduplicatePlan, String> {
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
        tasks.push(DeduplicateTask {
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

    Ok(DeduplicatePlan {
        tasks,
        type_summary,
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
        append_checkpoint(&staging_dir, "b.zip").unwrap();

        let loaded = load_checkpoint(&staging_dir).unwrap();
        assert!(loaded.contains("a.jpg"));
        assert!(loaded.contains("b.zip"));
        assert_eq!(loaded.len(), 2);
    }
}
