use std::path::PathBuf;

use crate::core::rclone::RcloneCommand as _;
use crate::core::rclone::command::{Copy, Purge};

pub(crate) struct RcloneCopyPlan {
    pub source: String,
    pub destination: String,
    pub log_path: PathBuf,
    pub transfers: usize,
    pub checkers: usize,
    pub tpslimit: Option<usize>,
}

#[derive(Debug, Default, Clone)]
pub(crate) struct RcloneCopySummary {
    pub transferred: u64,
    pub errors: u64,
    pub bytes: u64,
    pub log_path: PathBuf,
}

pub(crate) struct RcloneDeletePlan {
    pub source: String,
    pub log_path: PathBuf,
    pub checkers: usize,
}

#[derive(Debug, Default, Clone)]
pub(crate) struct RcloneDeleteSummary {
    pub deleted: u64,
    pub errors: u64,
    pub log_path: PathBuf,
}

/// Runs `rclone copy <source> <destination>` with `transfers`/`checkers`/
/// `tpslimit` resolved per-run (CLI-0108) and every other performance/retry
/// flag still fixed, plus the sandboxed `--config` every `core::rclone`
/// invocation gets (CLI-0136). Copy has no separate "local work" phase the
/// way download/hash/placement jobs do (CLI-0093's gauge was designed for
/// that split) -- its entire body of work *is* the transfer, so this sets
/// `pigeon_job_macro_phase` to "uploading" for the whole call, unlike
/// `transform`, which only flips it true once its own separate push phase
/// starts.
pub(crate) async fn run_copy_job(
    source: &str,
    destination: &str,
    log_path: &std::path::Path,
    transfers: usize,
    checkers: usize,
    tpslimit: Option<usize>,
) -> Result<RcloneCopySummary, String> {
    crate::observability::metrics::set_macro_phase("rclone-copy", true);

    let log_summary = Copy {
        source: source.to_string(),
        destination: destination.to_string(),
        log_path: log_path.to_path_buf(),
        transfers,
        checkers,
        tpslimit,
        job: "rclone-copy",
        step: "transfer",
        upload_metrics: true,
        ..Default::default()
    }
    .run()
    .await?;

    Ok(RcloneCopySummary {
        transferred: log_summary.transferred,
        errors: log_summary.errors,
        bytes: log_summary.bytes,
        log_path: log_path.to_path_buf(),
    })
}

/// Runs `rclone purge <source>` (CLI-0110) -- a recursive, irreversible
/// delete of everything under `source`. `--checkers` is still meaningful
/// (purge enumerates objects before deleting them) and `--fast-list` is
/// kept for the same reason (a listing optimization, not a transfer one).
/// `--transfers`/`--tpslimit` don't exist on this path at all -- no file
/// content moves, so no transfer concurrency or rate concept applies.
pub(crate) async fn run_delete_job(
    source: &str,
    log_path: &std::path::Path,
    checkers: usize,
) -> Result<RcloneDeleteSummary, String> {
    // Delete has no separate "local work" phase either -- same reasoning as
    // copy, just for enumeration+deletion instead of transfer.
    crate::observability::metrics::set_macro_phase("rclone-delete", true);

    let log_summary = Purge {
        source: source.to_string(),
        log_path: log_path.to_path_buf(),
        checkers,
        job: "rclone-delete",
        step: "delete",
    }
    .run()
    .await?;

    Ok(RcloneDeleteSummary {
        deleted: log_summary.deletes,
        errors: log_summary.errors,
        log_path: log_path.to_path_buf(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::rclone::check_available;

    /// `rclone copy` works against plain local filesystem paths. Skipped
    /// (not failed) if `rclone` genuinely isn't on `PATH`, since this whole
    /// job already refuses to run without it (`shared_wizard::prepare_rclone`) --
    /// this test would just be redundant with that failure on a machine
    /// that can't run it anyway.
    #[tokio::test]
    async fn copies_a_file_between_two_local_directories() {
        if check_available().await.is_err() {
            eprintln!("skipping: rclone not found on PATH");
            return;
        }

        let source_dir = tempfile::tempdir().unwrap();
        let dest_dir = tempfile::tempdir().unwrap();
        let log_dir = tempfile::tempdir().unwrap();
        std::fs::write(source_dir.path().join("hello.txt"), b"hello world").unwrap();

        let log_path = log_dir.path().join("rclone.jsonl");
        let summary = run_copy_job(
            source_dir.path().to_str().unwrap(),
            dest_dir.path().to_str().unwrap(),
            &log_path,
            2,
            4,
            None,
        )
        .await
        .unwrap();

        assert_eq!(summary.transferred, 1);
        assert_eq!(summary.errors, 0);
        assert!(dest_dir.path().join("hello.txt").exists());
    }

    /// A `Some(tpslimit)` must not break the rclone invocation -- the
    /// CLI-0106 default this CLI-0108 reverted is still a valid value to
    /// opt back into per-destination.
    #[tokio::test]
    async fn copies_a_file_with_a_tpslimit_set() {
        if check_available().await.is_err() {
            eprintln!("skipping: rclone not found on PATH");
            return;
        }

        let source_dir = tempfile::tempdir().unwrap();
        let dest_dir = tempfile::tempdir().unwrap();
        let log_dir = tempfile::tempdir().unwrap();
        std::fs::write(source_dir.path().join("hello.txt"), b"hello world").unwrap();

        let log_path = log_dir.path().join("rclone.jsonl");
        let summary = run_copy_job(
            source_dir.path().to_str().unwrap(),
            dest_dir.path().to_str().unwrap(),
            &log_path,
            8,
            16,
            Some(10),
        )
        .await
        .unwrap();

        assert_eq!(summary.transferred, 1);
        assert_eq!(summary.errors, 0);
        assert!(dest_dir.path().join("hello.txt").exists());
    }

    /// Real end-to-end `rclone purge` against a local directory tree,
    /// mirroring the copy tests above. Confirms purge actually removes
    /// both files and the directory structure itself (unlike rclone's own
    /// `delete` subcommand, which leaves empty directories behind).
    #[tokio::test]
    async fn purges_everything_under_a_local_directory() {
        if check_available().await.is_err() {
            eprintln!("skipping: rclone not found on PATH");
            return;
        }

        let source_dir = tempfile::tempdir().unwrap();
        let log_dir = tempfile::tempdir().unwrap();
        std::fs::write(source_dir.path().join("a.txt"), b"a").unwrap();
        std::fs::create_dir(source_dir.path().join("sub")).unwrap();
        std::fs::write(source_dir.path().join("sub/b.txt"), b"b").unwrap();

        let log_path = log_dir.path().join("rclone-purge.jsonl");
        let summary = run_delete_job(source_dir.path().to_str().unwrap(), &log_path, 4)
            .await
            .unwrap();

        assert_eq!(summary.errors, 0);
        assert!(summary.deleted >= 1);
        assert!(!source_dir.path().join("a.txt").exists());
        assert!(!source_dir.path().join("sub").exists());
    }
}
