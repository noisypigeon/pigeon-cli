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

/// How far a single archive's extracted bytes may exceed its own on-disk
/// (compressed) size before extraction is treated as a zip bomb (ADR-0098,
/// replacing a prior run-wide cumulative cap that punished a run for
/// containing a lot of *legitimately* large data spread across many
/// archives -- real buckets holding hundreds of GB of already-compressed
/// media zips tripped the old cap partway through and silently dropped
/// every member extracted afterward). A legitimate archive of
/// already-compressed media (photos, video) sits near a 1:1 ratio; even a
/// legitimately compressible archive of plain text rarely clears 10-20:1.
/// 100x is deliberately generous headroom while still catching a
/// pathological nested-zip bomb. `worker.rs`'s live disk-space check
/// (`download::check_disk_space`, ADR-0076) remains the real-time backstop
/// this guard doesn't need to duplicate.
const EXTRACTION_RATIO_LIMIT: u64 = 100;

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

/// `true` for a macOS AppleDouble resource-fork sidecar (`._<name>`,
/// anywhere in the archive) or anything under a `__MACOSX/` metadata
/// directory -- Finder-written noise that macOS zip tooling attaches
/// alongside real content, never the user's own data (ADR-0098). These are
/// skipped entirely rather than extracted: left unfiltered, a `._foo.zip`
/// stub gets requeued by the caller as a nested zip purely because of its
/// name, then fails to open ("Could not find EOCD") since it's actually a
/// few KB of resource-fork metadata, not a zip -- a false archive failure
/// that previously flipped a run's exit code for zero real data loss.
fn is_apple_metadata_entry(entry_name: &str) -> bool {
    if entry_name.starts_with("__MACOSX/") {
        return true;
    }
    entry_name
        .rsplit('/')
        .next()
        .unwrap_or(entry_name)
        .starts_with("._")
}

/// Streams every regular-file entry out of the zip at `zip_path` into a
/// fresh file under `raw_dir` (named via `counter`, the same monotonic
/// allocator `worker.rs` uses for every other scratch/raw file), in order.
/// Directory entries and AppleDouble/`__MACOSX` metadata entries
/// (`is_apple_metadata_entry`) are skipped. A password-protected or corrupt
/// archive is a plain `Err` -- treated by the caller as a failed task, not
/// a job abort. A member whose *actual* streamed byte count (not its
/// declared header size, which a corrupt or malicious archive can lie
/// about) would push this archive's own extraction past
/// `EXTRACTION_RATIO_LIMIT` times its on-disk size is truncated and
/// dropped. Returns the surviving members alongside how many were dropped
/// this way -- the caller counts this as a real failure (ADR-0098; a prior
/// version of this function only logged it, contradicting this same doc
/// comment).
pub(crate) fn expand_to_dir(
    zip_path: &Path,
    raw_dir: &Path,
    counter: &AtomicU64,
) -> Result<(Vec<ExtractedMember>, usize), String> {
    std::fs::create_dir_all(raw_dir)
        .map_err(|err| format!("failed to create {}: {err}", raw_dir.display()))?;
    let zip_file = File::open(zip_path)
        .map_err(|err| format!("failed to open {}: {err}", zip_path.display()))?;
    let compressed_size = zip_file
        .metadata()
        .map(|metadata| metadata.len())
        .unwrap_or(0);
    let budget = compressed_size.saturating_mul(EXTRACTION_RATIO_LIMIT);
    let mut archive =
        ZipArchive::new(zip_file).map_err(|err| format!("invalid zip archive: {err}"))?;

    let mut members = Vec::with_capacity(archive.len());
    let mut dropped = 0usize;
    let mut running_total = 0u64;
    for index in 0..archive.len() {
        let mut entry = archive
            .by_index(index)
            .map_err(|err| format!("failed to read zip entry {index}: {err}"))?;
        if entry.is_dir() {
            continue;
        }
        let name = entry.name().to_string();
        if is_apple_metadata_entry(&name) {
            tracing::debug!(
                member = %name,
                archive = %zip_path.display(),
                "skipping macOS AppleDouble/__MACOSX metadata entry"
            );
            continue;
        }
        let member_index = counter.fetch_add(1, Ordering::SeqCst);
        let member_path = raw_dir.join(format!("{member_index:012}.member"));
        let mut out_file = File::create(&member_path)
            .map_err(|err| format!("failed to create {}: {err}", member_path.display()))?;

        match copy_capped(&mut entry, &mut out_file, &mut running_total, budget) {
            Ok(size) => members.push(ExtractedMember {
                name,
                path: member_path,
                size,
            }),
            Err(err) => {
                let _ = std::fs::remove_file(&member_path);
                tracing::warn!(
                    member = %name,
                    archive = %zip_path.display(),
                    step = "archive",
                    error = %err,
                    "dropped zip member during extraction"
                );
                dropped += 1;
            }
        }
    }
    Ok((members, dropped))
}

/// Copies `reader` into `writer` in `COPY_CHUNK_BYTES` chunks, tracking
/// `running_total` (this archive's own extracted bytes so far -- extraction
/// within one `expand_to_dir` call is sequential, never concurrent, so a
/// plain counter suffices) and erroring out the moment the *real* running
/// total exceeds `budget` -- deliberately never trusts a zip entry's own
/// declared/header size for this check, since that's exactly what a
/// corrupted or malicious archive can lie about; only bytes genuinely
/// written to disk count.
fn copy_capped(
    reader: &mut impl Read,
    writer: &mut impl Write,
    running_total: &mut u64,
    budget: u64,
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
        *running_total += read as u64;
        if *running_total > budget {
            return Err(format!(
                "extraction cap ({budget} bytes, {EXTRACTION_RATIO_LIMIT}x this archive's compressed size) exceeded"
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

        let (members, dropped) = expand_to_dir(&zip_path, &raw_dir, &counter).unwrap();

        assert_eq!(dropped, 0);
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

        assert!(expand_to_dir(&zip_path, &raw_dir, &counter).is_err());
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

        let (members, _dropped) = expand_to_dir(&zip_path, &raw_dir, &counter).unwrap();

        assert_eq!(members.len(), 1);
        assert_eq!(members[0].name, "photos/img.jpg");
    }

    #[test]
    fn expand_to_dir_skips_apple_double_and_macosx_entries() {
        let dir = tempfile::tempdir().unwrap();
        let raw_dir = dir.path().join("raw");
        let zip_path = write_zip_file(
            dir.path(),
            &build_test_zip(&[
                ("real.txt", b"hello"),
                ("__MACOSX/._real.txt", b"resource-fork-junk"),
                // A stub named like a nested zip, but not actually one --
                // must never be treated as a zip to expand further, nor
                // counted as a failure.
                ("__MACOSX/nested/._archive.zip", b"resource-fork-junk"),
                ("._real.txt", b"resource-fork-junk"),
            ]),
        );
        let counter = AtomicU64::new(0);

        let (members, dropped) = expand_to_dir(&zip_path, &raw_dir, &counter).unwrap();

        assert_eq!(dropped, 0);
        assert_eq!(members.len(), 1);
        assert_eq!(members[0].name, "real.txt");
    }

    #[test]
    fn copy_capped_streams_and_reports_actual_bytes_written() {
        let source = b"hello world".to_vec();
        let mut reader = Cursor::new(source.clone());
        let mut sink = Vec::new();
        let mut total = 0u64;

        let written = copy_capped(&mut reader, &mut sink, &mut total, u64::MAX).unwrap();

        assert_eq!(written, source.len() as u64);
        assert_eq!(sink, source);
        assert_eq!(total, source.len() as u64);
    }

    #[test]
    fn copy_capped_enforces_the_cap_against_real_bytes_not_a_declared_size() {
        // Simulates a zip entry whose header lies about its size -- the cap
        // must trip on bytes actually streamed, regardless of what any
        // metadata claimed.
        let source = vec![0u8; 100];
        let mut reader = Cursor::new(source);
        let mut sink = Vec::new();
        let mut total = 0u64;

        let result = copy_capped(&mut reader, &mut sink, &mut total, 50);

        assert!(result.is_err());
    }

    #[test]
    fn expand_to_dir_drops_a_member_that_exceeds_this_archive_s_ratio_cap_but_keeps_others() {
        let dir = tempfile::tempdir().unwrap();
        let raw_dir = dir.path().join("raw");
        // A classic zip-bomb shape: one member that deflates to a tiny
        // on-disk footprint but decompresses to far more than
        // `EXTRACTION_RATIO_LIMIT` times the *whole archive's* on-disk
        // size -- unlike already-compressed real-world media (jpg/mp4),
        // which sits near a 1:1 ratio and is never affected by this guard.
        let mut buf = Vec::new();
        {
            let mut writer = ZipWriter::new(Cursor::new(&mut buf));
            let stored =
                SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
            let deflated =
                SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);
            writer.start_file("small.txt", stored).unwrap();
            writer.write_all(b"ok").unwrap();
            writer.start_file("bomb.bin", deflated).unwrap();
            writer.write_all(&vec![0u8; 1_000_000]).unwrap();
            writer.finish().unwrap();
        }
        let zip_path = write_zip_file(dir.path(), &buf);
        let counter = AtomicU64::new(0);

        let (members, dropped) = expand_to_dir(&zip_path, &raw_dir, &counter).unwrap();

        assert_eq!(dropped, 1);
        assert_eq!(members.len(), 1);
        assert_eq!(members[0].name, "small.txt");
    }
}
