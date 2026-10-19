//! Disk-space and content-hashing helpers shared by every job that works
//! with local files (originally the private helpers of the since-removed
//! `pull_transform::worker`, CLI-0076; hoisted here once `deduplicate`
//! needed the exact same behavior, CLI-0082 §0). Renamed from `download.rs`
//! (CLI-0136) once `deduplicate`'s data legs moved onto `core::rclone` and
//! this module's own download helpers (`download_with_retry`,
//! `DownloadAnnounce`) reached zero consumers and were deleted -- nothing
//! here downloads anything anymore, only `transform` (`sha256_hex`,
//! CLI-0112; `check_disk_space`, CLI-0130) and `deduplicate`
//! (`check_disk_space`, `sha256_file`) remain.

use std::fs::File;
use std::io::Read;
use std::path::Path;

use sha2::{Digest, Sha256};

/// A hard floor on free disk space a job insists on before starting a
/// download or a zip expansion (CLI-0076) -- disk, not memory, is the
/// resource a run can actually exhaust once everything is streamed rather
/// than buffered. Deliberately a flat safety margin, not a precise
/// prediction: a zip's expanded size isn't knowable up front, and a
/// too-clever estimate is worse than a simple one here.
pub(crate) const MIN_FREE_DISK_BYTES: u64 = 2 * 1024 * 1024 * 1024;

fn format_mb(bytes: u64) -> String {
    format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
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
/// actually run out of once everything is disk-streamed (CLI-0076). Fails
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

pub(crate) fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hex::encode(hasher.finalize())
}

/// Hashes a file by streaming it through `Sha256` in fixed-size chunks
/// (CLI-0076) -- avoids pulling a whole (potentially huge) file into memory
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
