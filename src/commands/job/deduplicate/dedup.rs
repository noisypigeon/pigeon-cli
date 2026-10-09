//! Content-hash dedup, placement, and merge-report generation (ADR-0082
//! §5/§6). Built directly on `core::data`'s primitives rather than reusing
//! `pull_transform::dedup::place_files` -- that function doesn't expose
//! per-duplicate merge records, which this job's report requires.

use std::fs;
use std::path::{Path, PathBuf};

use indicatif::MultiProgress;

use crate::commands::job::email_sync::sink;
use crate::commands::job::pull_transform::archive;
use crate::core::data::{ContentIndex, Dedup, after_zip_separator, sanitize_filename, unique_path};

pub(crate) const CONTENT_HASHES_FILE: &str = ".content-hashes";

pub(crate) struct DeduplicateDedup(pub(crate) ContentIndex);

impl Dedup for DeduplicateDedup {
    fn check(&self, hash: &str) -> Option<&str> {
        self.0.check(hash)
    }

    fn commit(&mut self, hash: &str, relative_path: &str) -> Result<(), String> {
        self.0.commit(hash, relative_path)
    }
}

/// One file finished the concurrent download/expand/hash pipeline,
/// awaiting placement -- `scratch_path` points at its bytes sitting outside
/// the final `result/<extension>/` tree. `bucket_alias` (ADR-0109) names
/// which of this run's (possibly several) source buckets `original_key` was
/// downloaded from.
#[derive(Debug)]
pub(crate) struct HashedFile {
    pub original_key: String,
    pub bucket_alias: String,
    pub scratch_path: PathBuf,
    pub extension: String,
    pub content_hash: String,
}

/// One duplicate discovered during placement, for the human-readable report
/// (`write_report`). `kept_original_key` (ADR-0099) is the source key of
/// whichever file was kept under `kept_path` -- blank when that hash was
/// committed before ADR-0099 (never retroactively backfilled, since a hash
/// is only ever committed once). `duplicate_bucket_alias`/
/// `kept_bucket_alias` (ADR-0109) name which source bucket the duplicate
/// and the kept copy respectively came from -- both blank under the same
/// pre-ADR-0109 circumstances as `kept_original_key`.
pub(crate) struct MergeRecord {
    pub duplicate_key: String,
    pub duplicate_bucket_alias: String,
    pub kept_path: String,
    pub kept_original_key: String,
    pub kept_bucket_alias: String,
    pub content_hash: String,
}

#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct PlacementSummary {
    pub placed: usize,
    pub duplicates_skipped: usize,
    pub failed: usize,
}

/// Whether `key`'s file name carries the source data's "no known date"
/// sentinel (`0000-00-00-...`) rather than a real date prefix. Deduplicate's
/// keep-selection (ADR-0095) uses this to avoid preferring an undated copy
/// over a dated one just because "0000" sorts before a real year.
fn is_undated_key(key: &str) -> bool {
    let key = after_zip_separator(key);
    let name = Path::new(key)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(key);
    name.starts_with("0000-00-00-")
}

/// Places every entry in `files` under `result_dir/<extension>/`,
/// deduplicating by content hash and recording a `MergeRecord` for every
/// duplicate found. Sorted with dated keys ahead of undated ones (ADR-0095),
/// then by `original_key` within each group for reproducible placement
/// order across re-runs, same discipline as every other single-threaded
/// placement pass in this codebase (concurrent `unique_path` calls against
/// a shared directory would race).
///
/// A single file's placement failure is logged and simply omitted from the
/// returned `finished_keys` list (retried next run), rather than aborting
/// the whole pass. Each `finished_keys` entry is `(bucket_alias, key)`
/// (ADR-0109), compound-keyed for the same collision reason as every other
/// cross-bucket tracking structure in this job (two source buckets can
/// share an identical key string).
pub(crate) fn place_and_report(
    result_dir: &Path,
    mut files: Vec<HashedFile>,
    dedup: &mut DeduplicateDedup,
    multi_progress: &MultiProgress,
) -> (PlacementSummary, Vec<MergeRecord>, Vec<(String, String)>) {
    files.sort_by(|a, b| {
        is_undated_key(&a.original_key)
            .cmp(&is_undated_key(&b.original_key))
            .then_with(|| a.original_key.cmp(&b.original_key))
    });

    let bar = sink::new_progress_bar("place".to_string(), files.len() as u64, multi_progress);
    let mut summary = PlacementSummary::default();
    let mut records = Vec::new();
    let mut finished_keys = Vec::with_capacity(files.len());

    for file in files {
        bar.inc(1);
        match dedup
            .0
            .check_with_original_key_and_bucket(&file.content_hash)
        {
            Some((kept_path, kept_original_key, kept_bucket_alias)) => {
                records.push(MergeRecord {
                    duplicate_key: file.original_key.clone(),
                    duplicate_bucket_alias: file.bucket_alias.clone(),
                    kept_path: kept_path.to_string(),
                    kept_original_key: kept_original_key.to_string(),
                    kept_bucket_alias: kept_bucket_alias.to_string(),
                    content_hash: file.content_hash.clone(),
                });
                let _ = fs::remove_file(&file.scratch_path);
                summary.duplicates_skipped += 1;
                finished_keys.push((file.bucket_alias, file.original_key));
            }
            None => {
                if let Err(err) = place_one(result_dir, &file, dedup) {
                    tracing::warn!(
                        key = %file.original_key,
                        step = "place",
                        error = %err,
                        "failed to place file"
                    );
                    summary.failed += 1;
                    crate::observability::metrics::record_phase(
                        "deduplicate",
                        "placement",
                        "failed",
                        None,
                    );
                    continue;
                }
                summary.placed += 1;
                crate::observability::metrics::record_phase("deduplicate", "placement", "ok", None);
                finished_keys.push((file.bucket_alias, file.original_key));
            }
        }
    }

    bar.finish();
    (summary, records, finished_keys)
}

fn place_one(
    result_dir: &Path,
    file: &HashedFile,
    dedup: &mut DeduplicateDedup,
) -> Result<(), String> {
    let extension_dir = result_dir.join(&file.extension);
    fs::create_dir_all(&extension_dir)
        .map_err(|err| format!("failed to create {}: {err}", extension_dir.display()))?;

    let key_tail = after_zip_separator(&file.original_key);
    let original_name = Path::new(key_tail)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(key_tail);
    let final_path = unique_path(&extension_dir.join(sanitize_filename(original_name)));

    fs::rename(&file.scratch_path, &final_path).map_err(|err| {
        format!(
            "failed to move {} to {}: {err}",
            file.scratch_path.display(),
            final_path.display()
        )
    })?;

    let relative_path = format!(
        "{}/{}",
        file.extension,
        final_path.file_name().unwrap().to_string_lossy()
    );
    dedup.0.commit_with_key_and_bucket(
        &file.content_hash,
        &relative_path,
        &file.original_key,
        &file.bucket_alias,
    )
}

/// Writes a plain-text, tab-separated merge report to
/// `local_output/deduplicate-report.txt` -- one line per `MergeRecord` plus a
/// trailing summary line. Written even when `records` is empty, so the
/// report lives at a predictable, scriptable path every run (ADR-0082 §5).
/// `kept_original_key` (ADR-0099) is blank whenever the kept copy's hash
/// predates that change -- see `MergeRecord`'s doc comment.
/// `duplicate_bucket_alias`/`kept_bucket_alias` (ADR-0109) are appended as
/// trailing columns rather than interspersed, so anything already parsing
/// this file's first four columns positionally keeps working.
pub(crate) fn write_report(
    local_output: &Path,
    records: &[MergeRecord],
    archive_failures: &[archive::ArchiveFailure],
) -> Result<(), String> {
    let mut contents = String::from(
        "duplicate_key\tcontent_hash\tkept_path\tkept_original_key\tduplicate_bucket_alias\tkept_bucket_alias\n",
    );
    for record in records {
        contents.push_str(&format!(
            "{}\t{}\t{}\t{}\t{}\t{}\n",
            record.duplicate_key,
            record.content_hash,
            record.kept_path,
            record.kept_original_key,
            record.duplicate_bucket_alias,
            record.kept_bucket_alias
        ));
    }
    contents.push_str(&format!("\n{} duplicate(s) removed.\n", records.len()));

    contents.push_str(&format!(
        "\n{} archive(s) need manual attention (password-protected or corrupt -- open and resolve by hand, then re-run):\n",
        archive_failures.len()
    ));
    for failure in archive_failures {
        contents.push_str(&format!("{}\t{}\n", failure.key, failure.error));
    }

    let path = local_output.join("deduplicate-report.txt");
    fs::write(&path, contents).map_err(|err| format!("failed to write {}: {err}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dedup_at(dir: &Path) -> DeduplicateDedup {
        DeduplicateDedup(ContentIndex::load(dir, CONTENT_HASHES_FILE).unwrap())
    }

    fn stage_scratch(staging: &Path, name: &str, contents: &[u8]) -> PathBuf {
        let path = staging.join(name);
        fs::write(&path, contents).unwrap();
        path
    }

    #[test]
    fn place_and_report_places_a_lone_file() {
        let staging = tempfile::tempdir().unwrap();
        let result_dir = tempfile::tempdir().unwrap();
        let mut dedup = dedup_at(staging.path());

        let file = HashedFile {
            original_key: "docs/report.pdf".to_string(),
            bucket_alias: "bucket-a".to_string(),
            scratch_path: stage_scratch(staging.path(), "scratch.pdf", b"pdf-bytes"),
            extension: "pdf".to_string(),
            content_hash: "hash-a".to_string(),
        };

        let (summary, records, finished_keys) = place_and_report(
            result_dir.path(),
            vec![file],
            &mut dedup,
            &MultiProgress::new(),
        );

        assert_eq!(summary.placed, 1);
        assert_eq!(summary.duplicates_skipped, 0);
        assert!(records.is_empty());
        assert_eq!(
            finished_keys,
            vec![("bucket-a".to_string(), "docs/report.pdf".to_string())]
        );
        assert!(result_dir.path().join("pdf/report.pdf").exists());
        assert_eq!(dedup.check("hash-a"), Some("pdf/report.pdf"));
    }

    #[test]
    fn place_and_report_strips_the_zip_member_prefix_from_an_extensionless_member() {
        let staging = tempfile::tempdir().unwrap();
        let result_dir = tempfile::tempdir().unwrap();
        let mut dedup = dedup_at(staging.path());

        let file = HashedFile {
            original_key: "archive.zip!README".to_string(),
            bucket_alias: "bucket-a".to_string(),
            scratch_path: stage_scratch(staging.path(), "scratch.bin", b"readme-bytes"),
            extension: "(none)".to_string(),
            content_hash: "hash-readme".to_string(),
        };

        let (summary, _records, finished_keys) = place_and_report(
            result_dir.path(),
            vec![file],
            &mut dedup,
            &MultiProgress::new(),
        );

        assert_eq!(summary.placed, 1);
        assert_eq!(
            finished_keys,
            vec![("bucket-a".to_string(), "archive.zip!README".to_string())]
        );
        assert!(result_dir.path().join("(none)/README").exists());
        assert!(!result_dir.path().join("zip!README").exists());
    }

    #[test]
    fn place_and_report_deduplicates_a_cross_key_duplicate_and_records_it() {
        let staging = tempfile::tempdir().unwrap();
        let result_dir = tempfile::tempdir().unwrap();
        let mut dedup = dedup_at(staging.path());

        // Two different source buckets sharing the same duplicate content --
        // exactly the cross-bucket scenario ADR-0109 adds support for.
        let files = vec![
            HashedFile {
                original_key: "a/report.pdf".to_string(),
                bucket_alias: "bucket-a".to_string(),
                scratch_path: stage_scratch(staging.path(), "a.pdf", b"same-bytes"),
                extension: "pdf".to_string(),
                content_hash: "same-hash".to_string(),
            },
            HashedFile {
                original_key: "b/report-copy.pdf".to_string(),
                bucket_alias: "bucket-b".to_string(),
                scratch_path: stage_scratch(staging.path(), "b.pdf", b"same-bytes"),
                extension: "pdf".to_string(),
                content_hash: "same-hash".to_string(),
            },
        ];

        let (summary, records, finished_keys) =
            place_and_report(result_dir.path(), files, &mut dedup, &MultiProgress::new());

        assert_eq!(summary.placed, 1);
        assert_eq!(summary.duplicates_skipped, 1);
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].duplicate_key, "b/report-copy.pdf");
        assert_eq!(records[0].duplicate_bucket_alias, "bucket-b");
        assert_eq!(records[0].kept_path, "pdf/report.pdf");
        assert_eq!(records[0].kept_original_key, "a/report.pdf");
        assert_eq!(records[0].kept_bucket_alias, "bucket-a");
        assert_eq!(records[0].content_hash, "same-hash");
        assert_eq!(finished_keys.len(), 2);
        assert_eq!(
            fs::read_dir(result_dir.path().join("pdf")).unwrap().count(),
            1
        );
    }

    #[test]
    fn is_undated_key_detects_the_no_known_date_sentinel() {
        assert!(is_undated_key("jpg/0000-00-00-image-370.jpg"));
        assert!(!is_undated_key("2018/jpg/2018-01-02-image-12.jpg"));
        assert!(!is_undated_key("a/report.pdf"));
    }

    #[test]
    fn is_undated_key_strips_the_zip_member_prefix_first() {
        assert!(is_undated_key("archive.zip!jpg/0000-00-00-image-370.jpg"));
        assert!(!is_undated_key(
            "archive.zip!2018/jpg/2018-01-02-image-12.jpg"
        ));
    }

    #[test]
    fn place_and_report_prefers_a_dated_key_over_an_undated_duplicate() {
        let staging = tempfile::tempdir().unwrap();
        let result_dir = tempfile::tempdir().unwrap();
        let mut dedup = dedup_at(staging.path());

        // "0000-00-00-..." sorts before "2018-01-02-..." lexicographically,
        // so listing the undated file first exercises the actual fix rather
        // than just restating it.
        let files = vec![
            HashedFile {
                original_key: "jpg/0000-00-00-image-370.jpg".to_string(),
                bucket_alias: "bucket-a".to_string(),
                scratch_path: stage_scratch(staging.path(), "undated.jpg", b"same-bytes"),
                extension: "jpg".to_string(),
                content_hash: "same-hash".to_string(),
            },
            HashedFile {
                original_key: "2018/jpg/2018-01-02-image-12.jpg".to_string(),
                bucket_alias: "bucket-a".to_string(),
                scratch_path: stage_scratch(staging.path(), "dated.jpg", b"same-bytes"),
                extension: "jpg".to_string(),
                content_hash: "same-hash".to_string(),
            },
        ];

        let (summary, records, finished_keys) =
            place_and_report(result_dir.path(), files, &mut dedup, &MultiProgress::new());

        assert_eq!(summary.placed, 1);
        assert_eq!(summary.duplicates_skipped, 1);
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].duplicate_key, "jpg/0000-00-00-image-370.jpg");
        assert_eq!(records[0].kept_path, "jpg/2018-01-02-image-12.jpg");
        assert_eq!(
            records[0].kept_original_key,
            "2018/jpg/2018-01-02-image-12.jpg"
        );
        assert_eq!(finished_keys.len(), 2);
        assert!(
            result_dir
                .path()
                .join("jpg/2018-01-02-image-12.jpg")
                .exists()
        );
    }

    #[test]
    fn place_and_report_disambiguates_a_same_run_name_collision() {
        let staging = tempfile::tempdir().unwrap();
        let result_dir = tempfile::tempdir().unwrap();
        let mut dedup = dedup_at(staging.path());

        let files = vec![
            HashedFile {
                original_key: "a/report.pdf".to_string(),
                bucket_alias: "bucket-a".to_string(),
                scratch_path: stage_scratch(staging.path(), "a.pdf", b"aaa"),
                extension: "pdf".to_string(),
                content_hash: "hash-a".to_string(),
            },
            HashedFile {
                original_key: "b/report.pdf".to_string(),
                bucket_alias: "bucket-a".to_string(),
                scratch_path: stage_scratch(staging.path(), "b.pdf", b"bbb"),
                extension: "pdf".to_string(),
                content_hash: "hash-b".to_string(),
            },
        ];

        let (summary, records, _finished_keys) =
            place_and_report(result_dir.path(), files, &mut dedup, &MultiProgress::new());

        assert_eq!(summary.placed, 2);
        assert!(records.is_empty());
        assert!(result_dir.path().join("pdf/report.pdf").exists());
        assert!(result_dir.path().join("pdf/report-2.pdf").exists());
    }

    #[test]
    fn write_report_includes_a_row_per_record_and_a_summary_line() {
        let dir = tempfile::tempdir().unwrap();
        let records = vec![MergeRecord {
            duplicate_key: "b/report-copy.pdf".to_string(),
            duplicate_bucket_alias: "bucket-b".to_string(),
            kept_path: "pdf/report.pdf".to_string(),
            kept_original_key: "a/report.pdf".to_string(),
            kept_bucket_alias: "bucket-a".to_string(),
            content_hash: "same-hash".to_string(),
        }];

        write_report(dir.path(), &records, &[]).unwrap();

        let contents = fs::read_to_string(dir.path().join("deduplicate-report.txt")).unwrap();
        assert!(contents.contains(
            "b/report-copy.pdf\tsame-hash\tpdf/report.pdf\ta/report.pdf\tbucket-b\tbucket-a"
        ));
        assert!(contents.contains("1 duplicate(s) removed."));
    }

    #[test]
    fn write_report_is_written_even_with_zero_duplicates() {
        let dir = tempfile::tempdir().unwrap();

        write_report(dir.path(), &[], &[]).unwrap();

        let contents = fs::read_to_string(dir.path().join("deduplicate-report.txt")).unwrap();
        assert!(contents.contains("0 duplicate(s) removed."));
    }

    #[test]
    fn write_report_lists_archive_failures_under_a_manual_followup_section() {
        let dir = tempfile::tempdir().unwrap();
        let failures = vec![archive::ArchiveFailure {
            key: "project-emails-snapshot-2025-12-18-views.zip".to_string(),
            error: "failed to read zip entry 0: unsupported Zip archive: Password required to decrypt file".to_string(),
        }];

        write_report(dir.path(), &[], &failures).unwrap();

        let contents = fs::read_to_string(dir.path().join("deduplicate-report.txt")).unwrap();
        assert!(contents.contains("1 archive(s) need manual attention"));
        assert!(contents.contains("project-emails-snapshot-2025-12-18-views.zip"));
        assert!(contents.contains("Password required to decrypt file"));
    }
}
