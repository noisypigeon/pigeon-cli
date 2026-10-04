//! Recursive bucket listing + extension-based type summary (ADR-0074 §3) --
//! no downloads happen here, only `list_objects`. Zip contents aren't known
//! at this point (nothing has been downloaded yet); the post-run summary is
//! what reports the true, post-expansion picture (same "manifest is an
//! estimate" precedent as `email_sync`'s ADR-0032/0034).

use std::collections::HashSet;
use std::fs;
use std::path::Path;

use crate::commands::keyring::bucket::client;
use crate::commands::keyring::bucket::store::BucketConfig;
pub(crate) use crate::core::data::extension_of;

/// One source object pending processing -- `Job::gather`'s result. Only
/// top-level bucket objects; zip members discovered during `run()` are
/// queued dynamically there, never appear in this list.
pub(crate) struct PullTask {
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

pub(crate) struct PullTransformPlan {
    pub tasks: Vec<PullTask>,
    pub type_summary: Vec<TypeSummary>,
}

/// A source object key is durably marked done here once `run()` has fully
/// handled it (itself, for a non-zip; its own successful expansion, for a
/// zip -- not necessarily every extracted member's placement, see
/// `worker::run_pull_transform_job`'s doc comment) -- same role as
/// `email_sync`'s per-UID checkpoint (ADR-0007/0019), letting a re-run skip
/// already-handled keys without re-downloading or re-transcoding them.
pub(crate) const PROCESSED_FILE_NAME: &str = ".processed";

pub(crate) fn load_checkpoint(local_output: &Path) -> Result<HashSet<String>, String> {
    let path = local_output.join(PROCESSED_FILE_NAME);
    let contents = match fs::read_to_string(&path) {
        Ok(contents) => contents,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(HashSet::new()),
        Err(err) => return Err(format!("failed to read {}: {err}", path.display())),
    };
    Ok(contents.lines().map(str::to_string).collect())
}

pub(crate) fn append_checkpoint(local_output: &Path, key: &str) -> Result<(), String> {
    use std::io::Write;
    let path = local_output.join(PROCESSED_FILE_NAME);
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
/// `local_output` checkpoint already marks done -- both the returned task
/// list and the summary table reflect only *pending* work, mirroring
/// `email_sync`'s manifest (its `PENDING` column has the same meaning).
pub(crate) async fn gather_pending(
    bucket_config: &BucketConfig,
    secret: &str,
    local_output: &Path,
) -> Result<PullTransformPlan, String> {
    let done = load_checkpoint(local_output)?;
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
        tasks.push(PullTask {
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

    Ok(PullTransformPlan {
        tasks,
        type_summary,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn load_checkpoint_is_empty_for_a_fresh_directory() {
        let dir = tempfile::tempdir().unwrap();
        assert!(load_checkpoint(dir.path()).unwrap().is_empty());
    }

    #[test]
    fn append_checkpoint_then_load_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        append_checkpoint(dir.path(), "a.jpg").unwrap();
        append_checkpoint(dir.path(), "b.zip").unwrap();

        let loaded = load_checkpoint(dir.path()).unwrap();
        assert!(loaded.contains("a.jpg"));
        assert!(loaded.contains("b.zip"));
        assert_eq!(loaded.len(), 2);
    }
}
