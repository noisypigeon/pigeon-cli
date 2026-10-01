//! Recursive, disk-streamed zip expansion (ADR-0076, replacing ADR-0074's
//! original fully-in-memory design). Every member is streamed straight from
//! the zip entry's own `Read` impl to a fresh file on disk in bounded
//! chunks -- never fully materialized as a `Vec<u8>` -- because a zip can
//! (and, in practice, has) contained enough members, or few enough absurdly
//! large ones, to exceed available RAM well before it exceeds available
//! disk. A zip-in-a-zip is just another `expand_to_dir` call over the
//! now-on-disk member.

use std::fs::File;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use zip::ZipArchive;

/// How many nested zip levels `worker::run_pull_transform_job` will expand
/// before giving up on that branch -- a zip bomb built from many small
/// nested zips still terminates instead of expanding forever.
pub(crate) const MAX_ZIP_DEPTH: u32 = 10;

/// Total bytes this job will expand from zip members across the whole run,
/// tracked by the caller (a shared counter, since expansion happens
/// concurrently). With expansion disk-streamed (ADR-0076), this is purely a
/// disk-space/zip-bomb guard now, not a memory-safety one -- raised well
/// past ADR-0074's original 10 GiB, which assumed everything extracted
/// stayed resident in RAM and is far too small for legitimate zips that are
/// themselves tens of gigabytes compressed. `worker.rs`'s live disk-space
/// check is the real-time backstop; this constant only needs to catch a
/// genuinely pathological compression-ratio zip bomb.
pub(crate) const MAX_TOTAL_EXTRACTED_BYTES: u64 = 500 * 1024 * 1024 * 1024;

/// Chunk size for streaming a zip entry to disk -- large enough to keep
/// syscall overhead low, small enough that memory use per in-flight
/// extraction is trivial regardless of the member's total size.
const COPY_CHUNK_BYTES: usize = 64 * 1024;

pub(crate) struct ExtractedMember {
    /// The member's own path inside the archive, e.g. `"photos/img.jpg"` --
    /// the caller joins this onto the zip's own display key to build a
    /// synthetic, human-readable key for the extracted file.
    pub name: String,
    /// Where the member's decompressed bytes were streamed to on disk.
    pub path: PathBuf,
    pub size: u64,
}

/// Streams every regular-file entry out of the zip at `zip_path` into a
/// fresh file under `raw_dir` (named via `counter`, the same monotonic
/// allocator `worker.rs` uses for every other scratch/raw file), in order.
/// Directory entries are skipped. A password-protected or corrupt archive
/// is a plain `Err` -- treated by the caller as a failed task, not a job
/// abort. A member whose *actual* streamed byte count (not its declared
/// header size, which a corrupt or malicious archive can lie about) would
/// push `extracted_bytes` past `MAX_TOTAL_EXTRACTED_BYTES` is truncated and
/// dropped -- tallied as a failure for that one member, not the whole zip.
pub(crate) fn expand_to_dir(
    zip_path: &Path,
    raw_dir: &Path,
    counter: &AtomicU64,
    extracted_bytes: &AtomicU64,
) -> Result<Vec<ExtractedMember>, String> {
    std::fs::create_dir_all(raw_dir)
        .map_err(|err| format!("failed to create {}: {err}", raw_dir.display()))?;
    let zip_file = File::open(zip_path)
        .map_err(|err| format!("failed to open {}: {err}", zip_path.display()))?;
    let mut archive =
        ZipArchive::new(zip_file).map_err(|err| format!("invalid zip archive: {err}"))?;

    let mut members = Vec::with_capacity(archive.len());
    for index in 0..archive.len() {
        let mut entry = archive
            .by_index(index)
            .map_err(|err| format!("failed to read zip entry {index}: {err}"))?;
        if entry.is_dir() {
            continue;
        }
        let name = entry.name().to_string();
        let member_index = counter.fetch_add(1, Ordering::SeqCst);
        let member_path = raw_dir.join(format!("{member_index:012}.member"));
        let mut out_file = File::create(&member_path)
            .map_err(|err| format!("failed to create {}: {err}", member_path.display()))?;

        match copy_capped(&mut entry, &mut out_file, extracted_bytes) {
            Ok(size) => members.push(ExtractedMember {
                name,
                path: member_path,
                size,
            }),
            Err(err) => {
                let _ = std::fs::remove_file(&member_path);
                tracing::warn!(
                    member = %name,
                    step = "archive",
                    error = %err,
                    "dropped zip member during extraction"
                );
            }
        }
    }
    Ok(members)
}

/// Copies `reader` into `writer` in `COPY_CHUNK_BYTES` chunks, incrementing
/// `total_extracted` (shared across every concurrent extraction this run)
/// after each chunk actually written and erroring out the moment the
/// *real* running total exceeds `MAX_TOTAL_EXTRACTED_BYTES` -- deliberately
/// never trusts a zip entry's own declared/header size for this check,
/// since that's exactly what a malicious or corrupted archive can lie
/// about; only bytes genuinely written to disk count.
fn copy_capped(
    reader: &mut impl Read,
    writer: &mut impl Write,
    total_extracted: &AtomicU64,
) -> Result<u64, String> {
    let mut buf = [0u8; COPY_CHUNK_BYTES];
    let mut written = 0u64;
    loop {
        let read = reader
            .read(&mut buf)
            .map_err(|err| format!("failed to read zip entry: {err}"))?;
        if read == 0 {
            return Ok(written);
        }
        writer
            .write_all(&buf[..read])
            .map_err(|err| format!("failed to write extracted member: {err}"))?;
        written += read as u64;
        let total = total_extracted.fetch_add(read as u64, Ordering::SeqCst) + read as u64;
        if total > MAX_TOTAL_EXTRACTED_BYTES {
            return Err(format!(
                "extraction cap ({MAX_TOTAL_EXTRACTED_BYTES} bytes) exceeded"
            ));
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use zip::ZipWriter;
    use zip::write::SimpleFileOptions;

    use super::*;

    fn build_test_zip(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut buf = Vec::new();
        {
            let mut writer = ZipWriter::new(Cursor::new(&mut buf));
            let options =
                SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
            for (name, contents) in entries {
                writer.start_file(*name, options).unwrap();
                writer.write_all(contents).unwrap();
            }
            writer.finish().unwrap();
        }
        buf
    }

    fn write_zip_file(dir: &Path, bytes: &[u8]) -> PathBuf {
        let path = dir.join("archive.zip");
        std::fs::write(&path, bytes).unwrap();
        path
    }

    #[test]
    fn expand_to_dir_extracts_every_member_to_disk_in_order() {
        let dir = tempfile::tempdir().unwrap();
        let raw_dir = dir.path().join("raw");
        let zip_path = write_zip_file(
            dir.path(),
            &build_test_zip(&[("a.txt", b"hello"), ("b.txt", b"world")]),
        );
        let counter = AtomicU64::new(0);
        let extracted_bytes = AtomicU64::new(0);

        let members = expand_to_dir(&zip_path, &raw_dir, &counter, &extracted_bytes).unwrap();

        assert_eq!(members.len(), 2);
        assert_eq!(members[0].name, "a.txt");
        assert_eq!(std::fs::read(&members[0].path).unwrap(), b"hello");
        assert_eq!(members[0].size, 5);
        assert_eq!(members[1].name, "b.txt");
        assert_eq!(std::fs::read(&members[1].path).unwrap(), b"world");
    }

    #[test]
    fn expand_to_dir_rejects_non_zip_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let raw_dir = dir.path().join("raw");
        let zip_path = write_zip_file(dir.path(), b"not a zip file");
        let counter = AtomicU64::new(0);
        let extracted_bytes = AtomicU64::new(0);

        assert!(expand_to_dir(&zip_path, &raw_dir, &counter, &extracted_bytes).is_err());
    }

    #[test]
    fn expand_to_dir_skips_directory_entries() {
        let dir = tempfile::tempdir().unwrap();
        let raw_dir = dir.path().join("raw");
        let mut buf = Vec::new();
        {
            let mut writer = ZipWriter::new(Cursor::new(&mut buf));
            writer
                .add_directory("photos/", SimpleFileOptions::default())
                .unwrap();
            writer
                .start_file("photos/img.jpg", SimpleFileOptions::default())
                .unwrap();
            writer.write_all(b"fake-jpeg-bytes").unwrap();
            writer.finish().unwrap();
        }
        let zip_path = write_zip_file(dir.path(), &buf);
        let counter = AtomicU64::new(0);
        let extracted_bytes = AtomicU64::new(0);

        let members = expand_to_dir(&zip_path, &raw_dir, &counter, &extracted_bytes).unwrap();

        assert_eq!(members.len(), 1);
        assert_eq!(members[0].name, "photos/img.jpg");
    }

    #[test]
    fn copy_capped_streams_and_reports_actual_bytes_written() {
        let source = b"hello world".to_vec();
        let mut reader = Cursor::new(source.clone());
        let mut sink = Vec::new();
        let total = AtomicU64::new(0);

        let written = copy_capped(&mut reader, &mut sink, &total).unwrap();

        assert_eq!(written, source.len() as u64);
        assert_eq!(sink, source);
        assert_eq!(total.load(Ordering::SeqCst), source.len() as u64);
    }

    #[test]
    fn copy_capped_enforces_the_cap_against_real_bytes_not_a_declared_size() {
        // Simulates a zip entry whose header lies about its size -- the cap
        // must trip on bytes actually streamed, regardless of what any
        // metadata claimed.
        let source = vec![0u8; 100];
        let mut reader = Cursor::new(source);
        let mut sink = Vec::new();
        // Start the shared counter just below the cap so this one member's
        // real bytes push it over.
        let total = AtomicU64::new(MAX_TOTAL_EXTRACTED_BYTES - 50);

        let result = copy_capped(&mut reader, &mut sink, &total);

        assert!(result.is_err());
    }
}
