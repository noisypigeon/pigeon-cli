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
/// queued dynamically there, never appear in this list. `bucket_alias`
/// (ADR-0109) names which of this run's (possibly several) source buckets
/// `key` was listed from.
pub(crate) struct DeduplicateTask {
    pub bucket_alias: String,
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

/// One checkpoint entry: `(bucket_alias, key)`, compound-keyed (ADR-0109) so
/// two different source buckets that happen to share an identical key don't
/// shadow each other. `bucket_alias == ""` is a legacy sentinel for a line
/// written before ADR-0109 (no tab at all, just the bare key) -- see
/// `is_checkpointed` for how that's treated on lookup.
pub(crate) fn load_checkpoint(staging_dir: &Path) -> Result<HashSet<(String, String)>, String> {
    let path = staging_dir.join(PROCESSED_FILE_NAME);
    let contents = match fs::read_to_string(&path) {
        Ok(contents) => contents,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(HashSet::new()),
        Err(err) => return Err(format!("failed to read {}: {err}", path.display())),
    };
    Ok(contents
        .lines()
        .map(|line| match line.split_once('\t') {
            Some((bucket_alias, key)) => (bucket_alias.to_string(), key.to_string()),
            None => (String::new(), line.to_string()),
        })
        .collect())
}

/// Whether `(bucket_alias, key)` is already checkpointed -- also matches a
/// pre-ADR-0109 legacy entry (`("", key)`, written back when every run had
/// exactly one source bucket) against *any* bucket_alias for that key, so a
/// single-bucket run's checkpoint stays resumable without needing to know
/// which single bucket it originally used.
pub(crate) fn is_checkpointed(
    done: &HashSet<(String, String)>,
    bucket_alias: &str,
    key: &str,
) -> bool {
    done.contains(&(bucket_alias.to_string(), key.to_string()))
        || done.contains(&(String::new(), key.to_string()))
}

pub(crate) fn append_checkpoint(
    staging_dir: &Path,
    bucket_alias: &str,
    key: &str,
) -> Result<(), String> {
    use std::io::Write;
    fs::create_dir_all(staging_dir)
        .map_err(|err| format!("failed to create {}: {err}", staging_dir.display()))?;
    let path = staging_dir.join(PROCESSED_FILE_NAME);
    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .map_err(|err| format!("failed to open {}: {err}", path.display()))?;
    writeln!(file, "{bucket_alias}\t{key}")
        .map_err(|err| format!("failed to write {}: {err}", path.display()))
}

/// Lists every object across every `(bucket_config, secret)` pair in
/// `source_buckets` (recursive, already paginated by `client::list_objects`
/// per bucket), classifies each by its key's extension for the summary
/// table (one combined, cross-bucket table -- not broken out per bucket,
/// matching this job's "treat as one set" design, ADR-0109), and excludes
/// anything the `staging_dir` checkpoint already marks done for that
/// specific bucket -- both the returned task list and the summary table
/// reflect only *pending* work, mirroring
/// `pull_transform::manifest::gather_pending`.
pub(crate) async fn gather_pending(
    source_buckets: &[(BucketConfig, String)],
    staging_dir: &Path,
) -> Result<DeduplicatePlan, String> {
    let done = load_checkpoint(staging_dir)?;

    let mut tasks = Vec::new();
    let mut by_extension: std::collections::BTreeMap<String, (usize, u64)> =
        std::collections::BTreeMap::new();

    for (bucket_config, secret) in source_buckets {
        let entries = client::list_objects(bucket_config, secret, "", true).await?;
        for entry in entries {
            if entry.is_prefix || is_checkpointed(&done, &bucket_config.alias, &entry.key) {
                continue;
            }
            let extension = extension_of(&entry.key);
            let bucket = by_extension.entry(extension).or_insert((0, 0));
            bucket.0 += 1;
            bucket.1 += entry.size;
            tasks.push(DeduplicateTask {
                bucket_alias: bucket_config.alias.clone(),
                key: entry.key,
                size: entry.size,
            });
        }
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
        append_checkpoint(&staging_dir, "bucket-a", "a.jpg").unwrap();
        append_checkpoint(&staging_dir, "bucket-b", "b.zip").unwrap();

        let loaded = load_checkpoint(&staging_dir).unwrap();
        assert!(loaded.contains(&("bucket-a".to_string(), "a.jpg".to_string())));
        assert!(loaded.contains(&("bucket-b".to_string(), "b.zip".to_string())));
        assert_eq!(loaded.len(), 2);
    }

    #[test]
    fn is_checkpointed_matches_the_exact_bucket_and_key() {
        let dir = tempfile::tempdir().unwrap();
        let staging_dir = dir.path().join(".staging");
        append_checkpoint(&staging_dir, "bucket-a", "shared.jpg").unwrap();

        let done = load_checkpoint(&staging_dir).unwrap();
        assert!(is_checkpointed(&done, "bucket-a", "shared.jpg"));
        // Same key, different bucket -- must NOT be treated as done; this is
        // exactly the collision ADR-0109 fixes.
        assert!(!is_checkpointed(&done, "bucket-b", "shared.jpg"));
    }

    #[test]
    fn is_checkpointed_treats_a_legacy_bucket_less_line_as_matching_any_bucket() {
        let dir = tempfile::tempdir().unwrap();
        let staging_dir = dir.path().join(".staging");
        fs::create_dir_all(&staging_dir).unwrap();
        // A pre-ADR-0109 checkpoint line: bare key, no tab.
        fs::write(staging_dir.join(PROCESSED_FILE_NAME), "legacy.jpg\n").unwrap();

        let done = load_checkpoint(&staging_dir).unwrap();
        assert!(is_checkpointed(&done, "any-bucket-alias", "legacy.jpg"));
    }
}
