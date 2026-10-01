//! Disk-space-aware, streaming download primitives shared by every job that
//! pulls bucket objects to local disk (originally `pull_transform::worker`'s
//! own private helpers, ADR-0076; hoisted here once `dedupe` needed the
//! exact same behavior, ADR-0082 §0 -- same "extract on a second real
//! consumer" reasoning as `commands::job::upload`'s own ADR-0074 hoist).

use std::fs::File;
use std::io::Read;
use std::path::Path;
use std::time::Duration;

use indicatif::MultiProgress;
use sha2::{Digest, Sha256};

use crate::commands::keyring::bucket::client;
use crate::commands::keyring::bucket::store::BucketConfig;
use crate::core::retry::retry_with_backoff;

const DOWNLOAD_RETRIES: usize = 3;
const DOWNLOAD_RETRY_BACKOFF: Duration = Duration::from_secs(2);

/// Below this size, a download is fast enough not to need its own named
/// call-out -- the overall progress bar already shows it completing
/// (ADR-0075). At or above it, a `Downloading <key> (<size>)...` line
/// explains why one item might be visibly slower than the rest.
const ANNOUNCE_DOWNLOAD_THRESHOLD_BYTES: u64 = 50 * 1024 * 1024;

/// A hard floor on free disk space a job insists on before starting a
/// download or a zip expansion (ADR-0076) -- disk, not memory, is the
/// resource a run can actually exhaust once everything is streamed rather
/// than buffered. Deliberately a flat safety margin, not a precise
/// prediction: a zip's expanded size isn't knowable up front, and a
/// too-clever estimate is worse than a simple one here.
pub(crate) const MIN_FREE_DISK_BYTES: u64 = 2 * 1024 * 1024 * 1024;

/// A plain `123.4 MB` label for a byte count -- these announcements are
/// only ever for large files, so this always renders in MB.
fn format_mb(bytes: u64) -> String {
    format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
}

/// Whether a download of `size` bytes is worth a named call-out (ADR-0075)
/// -- a pure predicate, kept separate from `download_with_retry` itself so
/// the threshold logic is unit-testable without capturing progress-bar
/// output.
fn should_announce_download(size: u64) -> bool {
    size >= ANNOUNCE_DOWNLOAD_THRESHOLD_BYTES
}

/// The available space (in bytes) on whichever disk backs `path`, matched
/// by the longest mount-point prefix -- `None` if that can't be determined
/// (e.g. `path` doesn't exist yet), in which case the caller doesn't block
/// on an unknown rather than failing safe-but-wrong.
pub(crate) fn available_disk_space(path: &Path) -> Option<u64> {
    let target = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    let disks = sysinfo::Disks::new_with_refreshed_list();
    disks
        .list()
        .iter()
        .filter(|disk| target.starts_with(disk.mount_point()))
        .max_by_key(|disk| disk.mount_point().as_os_str().len())
        .map(|disk| disk.available_space())
}

/// Fails fast, before starting a download or zip expansion, if the disk
/// backing `path` doesn't have at least `needed` (or `MIN_FREE_DISK_BYTES`,
/// whichever is larger) free -- disk space, not memory, is what a job can
/// actually run out of once everything is disk-streamed (ADR-0076). Fails
/// only the one item calling this, not the whole run.
pub(crate) fn check_disk_space(path: &Path, needed: u64) -> Result<(), String> {
    let Some(available) = available_disk_space(path) else {
        return Ok(());
    };
    let required = needed.max(MIN_FREE_DISK_BYTES);
    if available < required {
        return Err(format!(
            "not enough disk space: {} available, need at least {}",
            format_mb(available),
            format_mb(required)
        ));
    }
    Ok(())
}

/// Streams `key` straight to `dest_path` (ADR-0076) -- never buffers the
/// whole object in memory, so a 50-100GB object costs a small, fixed
/// amount of RAM regardless of its size.
pub(crate) async fn download_with_retry(
    bucket_config: &BucketConfig,
    secret: &str,
    key: &str,
    size: u64,
    dest_path: &Path,
    multi_progress: &MultiProgress,
) -> Result<(), String> {
    if should_announce_download(size) {
        let _ = multi_progress.println(format!("Downloading {key} ({})...", format_mb(size)));
    }
    retry_with_backoff(DOWNLOAD_RETRIES, DOWNLOAD_RETRY_BACKOFF, || {
        client::download_object_to_file(bucket_config, secret, key, dest_path)
    })
    .await?;
    Ok(())
}

pub(crate) fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hex::encode(hasher.finalize())
}

/// Hashes a file by streaming it through `Sha256` in fixed-size chunks
/// (ADR-0076) -- avoids pulling a whole (potentially huge) file into memory
/// purely to hash it.
pub(crate) fn sha256_file(path: &Path) -> Result<String, String> {
    let mut file =
        File::open(path).map_err(|err| format!("failed to open {}: {err}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let read = file
            .read(&mut buf)
            .map_err(|err| format!("failed to read {}: {err}", path.display()))?;
        if read == 0 {
            break;
        }
        hasher.update(&buf[..read]);
    }
    Ok(hex::encode(hasher.finalize()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_announce_download_is_false_below_the_threshold() {
        assert!(!should_announce_download(
            ANNOUNCE_DOWNLOAD_THRESHOLD_BYTES - 1
        ));
        assert!(!should_announce_download(1024));
    }

    #[test]
    fn should_announce_download_is_true_at_and_above_the_threshold() {
        assert!(should_announce_download(ANNOUNCE_DOWNLOAD_THRESHOLD_BYTES));
        assert!(should_announce_download(
            ANNOUNCE_DOWNLOAD_THRESHOLD_BYTES + 1
        ));
    }

    #[test]
    fn format_mb_renders_one_decimal_place() {
        assert_eq!(format_mb(50 * 1024 * 1024), "50.0 MB");
        assert_eq!(format_mb(1024 * 1024 + 512 * 1024), "1.5 MB");
    }

    #[test]
    fn sha256_file_matches_sha256_hex_for_identical_content() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.bin");
        std::fs::write(&path, b"hello world").unwrap();

        assert_eq!(sha256_file(&path).unwrap(), sha256_hex(b"hello world"));
    }

    #[test]
    fn available_disk_space_finds_a_positive_value_for_the_current_directory() {
        let dir = tempfile::tempdir().unwrap();
        let available = available_disk_space(dir.path());
        assert!(available.unwrap_or(0) > 0);
    }

    #[test]
    fn check_disk_space_fails_when_required_exceeds_available() {
        let dir = tempfile::tempdir().unwrap();
        // No real filesystem has an exbibyte free -- this must fail
        // regardless of the test machine's actual disk size.
        let result = check_disk_space(dir.path(), u64::MAX / 2);
        assert!(result.is_err());
    }

    #[test]
    fn check_disk_space_succeeds_for_a_small_requirement() {
        let dir = tempfile::tempdir().unwrap();
        assert!(check_disk_space(dir.path(), 1).is_ok());
    }
}
