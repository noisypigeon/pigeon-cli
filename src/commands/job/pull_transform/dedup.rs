//! Content-hash dedup and the sequential placement pass (ADR-0074 §5).
//! Wraps `core::data::ContentIndex`/`Dedup` exactly like
//! `email_sync::dedup::EmailDedup` does, keyed by SHA-256 (the ask was
//! explicitly SHA-based dedup; `ContentIndex` is hash-agnostic, so no core
//! change is needed) instead of email's MD5. Placement itself is
//! single-threaded for the same reason `email_sync`'s dedup/placement pass
//! is (ADR-0021's addendum): concurrent `unique_path`-style counter
//! assignment on the same target directory would race.

use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;

use indicatif::MultiProgress;

use crate::commands::job::email_sync::sink;
use crate::core::data::{ContentIndex, Dedup, after_zip_separator, sanitize_filename, unique_path};

use super::date::SimpleDate;

pub(crate) const CONTENT_HASHES_FILE: &str = ".content-hashes";

pub(crate) struct PullTransformDedup(pub(crate) ContentIndex);

impl Dedup for PullTransformDedup {
    fn check(&self, hash: &str) -> Option<&str> {
        self.0.check(hash)
    }

    fn commit(&mut self, hash: &str, relative_path: &str) -> Result<(), String> {
        self.0.commit(hash, relative_path)
    }
}

/// One file that finished the concurrent download/classify/recode/verify
/// pipeline, awaiting placement -- `scratch_path` points at its final bytes
/// (recoded, or the untouched original on a recode/verify fallback) sitting
/// outside the final `<local-output>/<extension>/` tree.
pub(crate) struct ProcessedFile {
    pub original_key: String,
    pub scratch_path: PathBuf,
    pub extension: String,
    pub date: Option<SimpleDate>,
    pub content_hash: String,
    pub is_media: bool,
}

#[derive(Debug, Default)]
pub(crate) struct PlacementSummary {
    pub placed: usize,
    pub duplicates_skipped: usize,
    pub failed: usize,
}

/// Places every entry in `files` under `local_output/<extension>/`,
/// skipping content-hash duplicates and assigning media files their
/// `{date}-{n}.{ext}` name (`n` a per-`(extension, date)` counter starting
/// at 1, incrementing for each additional file that day -- always numbered,
/// even the first, so a same-day file added by a later run never forces a
/// rename). Non-media files keep their sanitized original name,
/// disambiguated with `unique_path` on same-run collisions. Sorted by
/// `original_key` first for reproducible placement order across re-runs.
///
/// A single file's placement failure (e.g. a filesystem error moving one
/// scratch file) is tallied and skipped rather than aborting every
/// remaining file in the batch -- returns the `original_key` of every file
/// that finished (placed *or* recognized as a duplicate) so the caller can
/// checkpoint precisely those and let anything else retry next run.
pub(crate) fn place_files(
    local_output: &std::path::Path,
    mut files: Vec<ProcessedFile>,
    dedup: &mut PullTransformDedup,
    multi_progress: &MultiProgress,
) -> (PlacementSummary, Vec<String>) {
    files.sort_by(|a, b| a.original_key.cmp(&b.original_key));

    let bar = sink::new_progress_bar("place".to_string(), files.len() as u64, multi_progress);
    let mut summary = PlacementSummary::default();
    let mut per_day_counters: HashMap<(String, String), u32> = HashMap::new();
    let mut finished_keys = Vec::with_capacity(files.len());

    for file in files {
        if dedup.check(&file.content_hash).is_some() {
            let _ = fs::remove_file(&file.scratch_path);
            summary.duplicates_skipped += 1;
            finished_keys.push(file.original_key);
            bar.inc(1);
            continue;
        }

        if let Err(err) = place_one(local_output, &file, dedup, &mut per_day_counters) {
            tracing::warn!(
                key = %file.original_key,
                step = "place",
                error = %err,
                "failed to place file"
            );
            summary.failed += 1;
            crate::observability::metrics::record_phase(
                "pull-transform",
                "placement",
                "failed",
                None,
            );
            bar.inc(1);
            continue;
        }
        summary.placed += 1;
        crate::observability::metrics::record_phase("pull-transform", "placement", "ok", None);
        finished_keys.push(file.original_key);
        bar.inc(1);
    }

    bar.finish();
    (summary, finished_keys)
}

/// The literal folder-safe date label media files with no discoverable
/// date get grouped under (ADR-0074 §4's fallback chain), instead of a
/// fabricated placeholder date that would sort in among real ones.
const UNKNOWN_DATE_LABEL: &str = "unknown-date";

fn place_one(
    local_output: &std::path::Path,
    file: &ProcessedFile,
    dedup: &mut PullTransformDedup,
    per_day_counters: &mut HashMap<(String, String), u32>,
) -> Result<(), String> {
    let extension_dir = local_output.join(&file.extension);
    fs::create_dir_all(&extension_dir)
        .map_err(|err| format!("failed to create {}: {err}", extension_dir.display()))?;

    let final_path = if file.is_media {
        let date_label = file
            .date
            .map(|date| date.to_string())
            .unwrap_or_else(|| UNKNOWN_DATE_LABEL.to_string());
        let counter = per_day_counters
            .entry((file.extension.clone(), date_label.clone()))
            .or_insert(0);
        *counter += 1;
        extension_dir.join(format!("{date_label}-{counter}.{}", file.extension))
    } else {
        let key_tail = after_zip_separator(&file.original_key);
        let original_name = std::path::Path::new(key_tail)
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or(key_tail);
        unique_path(&extension_dir.join(sanitize_filename(original_name)))
    };

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
    dedup.commit(&file.content_hash, &relative_path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dedup_at(dir: &std::path::Path) -> PullTransformDedup {
        PullTransformDedup(ContentIndex::load(dir, CONTENT_HASHES_FILE).unwrap())
    }

    fn stage_scratch(staging: &std::path::Path, name: &str, contents: &[u8]) -> PathBuf {
        let path = staging.join(name);
        fs::write(&path, contents).unwrap();
        path
    }

    #[test]
    fn place_files_names_media_with_date_and_starting_at_one() {
        let output = tempfile::tempdir().unwrap();
        let staging = tempfile::tempdir().unwrap();
        let mut dedup = dedup_at(staging.path());

        let file = ProcessedFile {
            original_key: "photo1.jpg".to_string(),
            scratch_path: stage_scratch(staging.path(), "scratch1.jpg", b"photo-bytes"),
            extension: "jpg".to_string(),
            date: Some(SimpleDate {
                year: 2024,
                month: 1,
                day: 26,
            }),
            content_hash: "hash1".to_string(),
            is_media: true,
        };

        let (summary, _finished_keys) =
            place_files(output.path(), vec![file], &mut dedup, &MultiProgress::new());

        assert_eq!(summary.placed, 1);
        assert!(output.path().join("jpg/2024-01-26-1.jpg").exists());
    }

    #[test]
    fn place_files_increments_counter_for_same_day_media() {
        let output = tempfile::tempdir().unwrap();
        let staging = tempfile::tempdir().unwrap();
        let mut dedup = dedup_at(staging.path());
        let date = Some(SimpleDate {
            year: 2024,
            month: 1,
            day: 26,
        });

        let files = vec![
            ProcessedFile {
                original_key: "a.jpg".to_string(),
                scratch_path: stage_scratch(staging.path(), "a.jpg", b"aaa"),
                extension: "jpg".to_string(),
                date,
                content_hash: "hash-a".to_string(),
                is_media: true,
            },
            ProcessedFile {
                original_key: "b.jpg".to_string(),
                scratch_path: stage_scratch(staging.path(), "b.jpg", b"bbb"),
                extension: "jpg".to_string(),
                date,
                content_hash: "hash-b".to_string(),
                is_media: true,
            },
        ];

        let (summary, _finished_keys) =
            place_files(output.path(), files, &mut dedup, &MultiProgress::new());

        assert_eq!(summary.placed, 2);
        assert!(output.path().join("jpg/2024-01-26-1.jpg").exists());
        assert!(output.path().join("jpg/2024-01-26-2.jpg").exists());
    }

    #[test]
    fn place_files_skips_content_duplicates() {
        let output = tempfile::tempdir().unwrap();
        let staging = tempfile::tempdir().unwrap();
        let mut dedup = dedup_at(staging.path());
        let date = Some(SimpleDate {
            year: 2024,
            month: 1,
            day: 26,
        });

        let files = vec![
            ProcessedFile {
                original_key: "a.jpg".to_string(),
                scratch_path: stage_scratch(staging.path(), "a.jpg", b"same-bytes"),
                extension: "jpg".to_string(),
                date,
                content_hash: "same-hash".to_string(),
                is_media: true,
            },
            ProcessedFile {
                original_key: "b.jpg".to_string(),
                scratch_path: stage_scratch(staging.path(), "b.jpg", b"same-bytes"),
                extension: "jpg".to_string(),
                date,
                content_hash: "same-hash".to_string(),
                is_media: true,
            },
        ];

        let (summary, _finished_keys) =
            place_files(output.path(), files, &mut dedup, &MultiProgress::new());

        assert_eq!(summary.placed, 1);
        assert_eq!(summary.duplicates_skipped, 1);
        assert!(output.path().join("jpg/2024-01-26-1.jpg").exists());
    }

    #[test]
    fn place_files_keeps_sanitized_original_name_for_non_media() {
        let output = tempfile::tempdir().unwrap();
        let staging = tempfile::tempdir().unwrap();
        let mut dedup = dedup_at(staging.path());

        let file = ProcessedFile {
            original_key: "docs/report.pdf".to_string(),
            scratch_path: stage_scratch(staging.path(), "scratch.pdf", b"pdf-bytes"),
            extension: "pdf".to_string(),
            date: None,
            content_hash: "hash-doc".to_string(),
            is_media: false,
        };

        let (summary, _finished_keys) =
            place_files(output.path(), vec![file], &mut dedup, &MultiProgress::new());

        assert_eq!(summary.placed, 1);
        assert!(output.path().join("pdf/report.pdf").exists());
    }

    #[test]
    fn place_files_strips_the_zip_member_prefix_from_a_non_media_name() {
        let output = tempfile::tempdir().unwrap();
        let staging = tempfile::tempdir().unwrap();
        let mut dedup = dedup_at(staging.path());

        let file = ProcessedFile {
            original_key: "archive.zip!README".to_string(),
            scratch_path: stage_scratch(staging.path(), "scratch.bin", b"readme-bytes"),
            extension: "(none)".to_string(),
            date: None,
            content_hash: "hash-readme".to_string(),
            is_media: false,
        };

        let (summary, _finished_keys) =
            place_files(output.path(), vec![file], &mut dedup, &MultiProgress::new());

        assert_eq!(summary.placed, 1);
        assert!(output.path().join("(none)/README").exists());
        assert!(!output.path().join("zip!README").exists());
    }

    #[test]
    fn place_files_disambiguates_non_media_name_collisions() {
        let output = tempfile::tempdir().unwrap();
        let staging = tempfile::tempdir().unwrap();
        let mut dedup = dedup_at(staging.path());

        let files = vec![
            ProcessedFile {
                original_key: "a/report.pdf".to_string(),
                scratch_path: stage_scratch(staging.path(), "a.pdf", b"aaa"),
                extension: "pdf".to_string(),
                date: None,
                content_hash: "hash-a".to_string(),
                is_media: false,
            },
            ProcessedFile {
                original_key: "b/report.pdf".to_string(),
                scratch_path: stage_scratch(staging.path(), "b.pdf", b"bbb"),
                extension: "pdf".to_string(),
                date: None,
                content_hash: "hash-b".to_string(),
                is_media: false,
            },
        ];

        let (summary, _finished_keys) =
            place_files(output.path(), files, &mut dedup, &MultiProgress::new());

        assert_eq!(summary.placed, 2);
        assert!(output.path().join("pdf/report.pdf").exists());
        assert!(output.path().join("pdf/report-2.pdf").exists());
    }
}
