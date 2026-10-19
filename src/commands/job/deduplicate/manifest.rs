//! Recursive bucket listing + extension-based type summary (CLI-0082 §3) --
//! no downloads happen here, only `list_objects`. Originally its own copy
//! of the since-removed `pull_transform::manifest`'s pattern (that module
//! was private, unreachable from this sibling job), with one deliberate
//! layout correction: checkpoint
//! functions take `staging_dir`, not `local_output` itself -- keeps
//! `.processed`/`.content-hashes` out of the uploaded `result/` tree (see
//! `deduplicate/mod.rs`'s top-level doc comment).

use std::collections::HashSet;
use std::fs;
use std::path::Path;

use crate::commands::keyring::bucket::store::BucketConfig;
use crate::core::rclone::RcloneCommand as _;
use crate::core::rclone::command::Lsjson;
use crate::core::rclone::config::RemoteTarget;

/// The portion of `key` after its last `!` zip-member separator -- zip
/// expansion (`deduplicate::worker`) builds a synthetic,
/// human-readable key for an extracted member by joining the zip's own key
/// onto the member's in-archive path with `!` (e.g.
/// `"outer.zip!photos/img.jpg"`). `extension_of` and any filename-deriving
/// `Path` split must look at this, not the raw key, or a zip member's
/// synthetic prefix leaks into the derived extension/name (CLI-0099) --
/// e.g. an extensionless member `outer.zip!README` would otherwise be seen
/// as having extension `"zip!README"`, the only dot in the whole key.
/// Returns `key` unchanged when there's no `!`.
pub(crate) fn after_zip_separator(key: &str) -> &str {
    key.rsplit('!').next().unwrap_or(key)
}

/// The lowercased extension of `key`'s final path segment, or `"(none)"`
/// when there isn't one. Lived in `core::data` while `pull_transform`,
/// `deduplicate`, and `reduce` all needed it (hoisted by CLI-0096 §0);
/// returned here by CLI-0131 once the other two were removed and
/// `deduplicate` was its only remaining consumer -- the same un-hoisting
/// CLI-0094 did to `ConcurrencyInput`.
pub(crate) fn extension_of(key: &str) -> String {
    Path::new(after_zip_separator(key))
        .extension()
        .and_then(|ext| ext.to_str())
        .map(|ext| ext.to_ascii_lowercase())
        .unwrap_or_else(|| "(none)".to_string())
}

/// One source object pending processing -- `Job::gather`'s result. Only
/// top-level bucket objects; zip members discovered during `run()` are
/// queued dynamically there, never appear in this list. `bucket_alias`
/// (CLI-0109) names which of this run's (possibly several) source buckets
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
/// zip). Scoped to `staging_dir` rather than `local_output`, so it never
/// lands in the uploaded `result/` tree.
pub(crate) const PROCESSED_FILE_NAME: &str = ".processed";

/// One checkpoint entry: `(bucket_alias, key)`, compound-keyed (CLI-0109) so
/// two different source buckets that happen to share an identical key don't
/// shadow each other. `bucket_alias == ""` is a legacy sentinel for a line
/// written before CLI-0109 (no tab at all, just the bare key) -- see
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
/// pre-CLI-0109 legacy entry (`("", key)`, written back when every run had
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

/// Lists every object across every bucket-config in `source_buckets`
/// (recursive, metadata-only `rclone lsjson` against the sandboxed config,
/// CLI-0136), classifies each by its key's extension for the summary table
/// (one combined, cross-bucket table -- not broken out per bucket, matching
/// this job's "treat as one set" design, CLI-0109), and excludes anything
/// the `staging_dir` checkpoint already marks done for that specific bucket
/// -- both the returned task list and the summary table reflect only
/// *pending* work. The second tuple element (the resolved secret) is
/// unused here -- rclone reads every bucket's secret from the sandboxed
/// config file directly, not from this value.
pub(crate) async fn gather_pending(
    source_buckets: &[(BucketConfig, String)],
    staging_dir: &Path,
) -> Result<DeduplicatePlan, String> {
    let done = load_checkpoint(staging_dir)?;

    let mut tasks = Vec::new();
    let mut by_extension: std::collections::BTreeMap<String, (usize, u64)> =
        std::collections::BTreeMap::new();

    for (bucket_config, _secret) in source_buckets {
        let entries = (Lsjson {
            path: RemoteTarget::bucket_root(&bucket_config.alias).address(),
            recursive: true,
            // `missing_ok` stays `false` (CLI-0140): a source bucket rclone
            // cannot resolve must fail the run, not silently contribute
            // zero files to the manifest.
            job: "deduplicate",
            ..Default::default()
        })
        .run()
        .await?;
        for entry in entries {
            if is_checkpointed(&done, &bucket_config.alias, &entry.path) {
                continue;
            }
            let extension = extension_of(&entry.path);
            let bucket = by_extension.entry(extension).or_insert((0, 0));
            bucket.0 += 1;
            bucket.1 += entry.size;
            tasks.push(DeduplicateTask {
                bucket_alias: bucket_config.alias.clone(),
                key: entry.path,
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
    fn extension_of_lowercases_and_strips_the_dot() {
        assert_eq!(extension_of("Photos/IMG_0001.JPG"), "jpg");
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
    fn extension_of_strips_a_zip_member_prefix_before_deriving_the_extension() {
        assert_eq!(extension_of("outer.zip!README"), "(none)");
        assert_eq!(extension_of("outer.zip!photos/img.JPG"), "jpg");
    }

    #[test]
    fn after_zip_separator_returns_the_segment_after_the_last_bang() {
        assert_eq!(after_zip_separator("outer.zip!README"), "README");
        assert_eq!(
            after_zip_separator("a.zip!nested.zip!photos/img.jpg"),
            "photos/img.jpg"
        );
        assert_eq!(after_zip_separator("plain/key.jpg"), "plain/key.jpg");
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
        // exactly the collision CLI-0109 fixes.
        assert!(!is_checkpointed(&done, "bucket-b", "shared.jpg"));
    }

    #[test]
    fn is_checkpointed_treats_a_legacy_bucket_less_line_as_matching_any_bucket() {
        let dir = tempfile::tempdir().unwrap();
        let staging_dir = dir.path().join(".staging");
        fs::create_dir_all(&staging_dir).unwrap();
        // A pre-CLI-0109 checkpoint line: bare key, no tab.
        fs::write(staging_dir.join(PROCESSED_FILE_NAME), "legacy.jpg\n").unwrap();

        let done = load_checkpoint(&staging_dir).unwrap();
        assert!(is_checkpointed(&done, "any-bucket-alias", "legacy.jpg"));
    }
}
