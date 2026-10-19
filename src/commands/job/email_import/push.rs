//! Per-object push via `rclone copyto` (CLI-0146 §5), following
//! `transform/push.rs`'s shape.
//!
//! `copyto` is rclone's single-file-to-single-file-path primitive: no
//! directory listing round trip for one named object, unlike the bulk-tree
//! `Copy`. Pushing each message as it is fetched, rather than after the
//! whole corpus is local, is what lets a multi-day run deliver
//! incrementally and keeps local disk bounded to the in-flight window.

use std::path::Path;

use crate::core::rclone::RcloneCommand as _;
use crate::core::rclone::command::CopyTo;

/// Pushes one local file to `<destination_address><key>`.
///
/// `destination_address` is `RemoteTarget::address()`, which guarantees a
/// trailing `/` whenever a prefix is present, so `key` composes without a
/// separator of its own.
///
/// `push_log_dir` holds one small JSON log per object -- never one shared
/// path, since many concurrent single-file `rclone` subprocesses cannot
/// safely share a `--log-file`. The log name is derived from `key` with its
/// separators flattened, so a nested key cannot write outside that
/// directory.
pub(crate) async fn push_one(
    local_path: &Path,
    destination_address: &str,
    key: &str,
    push_log_dir: &Path,
) -> Result<(), String> {
    // Read before the push: CLI-0146 §5's GC deletes this file immediately
    // afterwards, so its size is unavailable then and `CopyTo` needs it for
    // `pigeon_upload_bytes_total`.
    let bytes = std::fs::metadata(local_path)
        .map(|meta| meta.len())
        .unwrap_or(0);

    let log_path = push_log_dir.join(format!("{}.jsonl", log_name_for(key)));

    (CopyTo {
        source: local_path.display().to_string(),
        destination: format!("{destination_address}{key}"),
        log_path,
        job: "email-import",
        step: "push",
        emit_upload_metrics: true,
        bytes,
    })
    .run()
    .await
}

/// Flattens an object key into a single safe filename component. Without
/// this a key like `inbox/1/attachments/a.pdf` would be taken as a nested
/// path and write its log into directories that do not exist.
fn log_name_for(key: &str) -> String {
    key.chars()
        .map(|ch| match ch {
            '/' | '\\' | ':' => '-',
            other => other,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn log_name_for_flattens_every_path_separator() {
        assert_eq!(
            log_name_for("a@b.com/inbox/1/attachments/a.pdf"),
            "a@b.com-inbox-1-attachments-a.pdf"
        );
        assert_eq!(log_name_for("1.eml"), "1.eml");
    }

    /// `rclone copyto` works against plain local filesystem paths. Skipped
    /// (not failed) when `rclone` genuinely is not on `PATH`, since the job
    /// already refuses to run without it via
    /// `shared_wizard::prepare_rclone` -- the same guard
    /// `transform/push.rs` and `rclone/worker.rs` use.
    #[tokio::test]
    async fn push_one_copies_a_file_to_a_local_destination_path() {
        if crate::core::rclone::check_available().await.is_err() {
            eprintln!("skipping: rclone not found on PATH");
            return;
        }

        let source_dir = tempfile::tempdir().unwrap();
        let dest_dir = tempfile::tempdir().unwrap();
        let local_path = source_dir.path().join("1.eml");
        std::fs::write(&local_path, b"raw message bytes").unwrap();

        // A trailing slash, as `RemoteTarget::address()` always produces
        // when a prefix is present.
        let destination = format!("{}/", dest_dir.path().display());

        push_one(
            &local_path,
            &destination,
            "a@b.com/inbox/1.eml",
            &source_dir.path().join("push-logs"),
        )
        .await
        .unwrap();

        assert_eq!(
            std::fs::read(dest_dir.path().join("a@b.com/inbox/1.eml")).unwrap(),
            b"raw message bytes"
        );
    }
}
