use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};

use bytes::Bytes;
use futures::StreamExt;
use minio::s3::MinioClient;
use minio::s3::builders::ObjectContent;
use minio::s3::creds::StaticProvider;
use minio::s3::error::{Error as S3Error, S3ServerError};
use minio::s3::http::BaseUrl;
use minio::s3::minio_error_response::MinioErrorCode;
use minio::s3::response_traits::HasEtagFromHeaders;
use minio::s3::segmented_bytes::SegmentedBytes;
use minio::s3::types::{S3Api, ToStream};

use crate::commands::keyring::bucket::store::BucketConfig;

/// The multipart part size pigeon always explicitly requests from the S3
/// client for a `Path`-backed upload (ADR-0089), pinned here rather than
/// left to the `minio` crate's own internal default so pigeon's own
/// locally-recomputed ETag (`expected_etag_for_file`) can never silently
/// drift from whatever the client actually uploaded, independent of
/// whether that crate-internal default ever changes in a future version.
/// Only ever passed to `put_object_content` when the file actually needs
/// multipart (`size > UPLOAD_PART_SIZE`) -- see `expected_etag_for_file`'s
/// doc comment for why passing it unconditionally is unsafe.
pub(crate) const UPLOAD_PART_SIZE: u64 = 64 * 1024 * 1024;

/// Read-buffer size for streaming a file through MD5 -- fixed and small
/// regardless of the file's own size, so hashing a 30GB file costs a few
/// MiB of RAM, not 30GB of it (ADR-0089).
const HASH_CHUNK_BYTES: usize = 8 * 1024 * 1024;

/// A listed S3 object or (in non-recursive/`lsd` mode) common prefix.
pub struct ObjectEntry {
    pub key: String,
    pub size: u64,
    pub is_prefix: bool,
}

/// Outcome of `upload_if_changed`'s existing-object hash comparison.
pub(crate) enum UploadOutcome {
    Uploaded,
    Unchanged,
}

/// The body to upload (ADR-0089): a plaintext file read straight off disk
/// and streamed (never fully materialized in memory, no matter its size),
/// or an already-encrypted in-memory buffer. `Bytes` exists because
/// ADR-0025's AES-256-GCM-SIV encryption is whole-buffer and only ever
/// sees email-sized files -- there's no plaintext-path equivalent to stream
/// for an encrypted upload. Cloning either variant is cheap (a `PathBuf`
/// copy, or a `Bytes` refcount bump), which is what lets a retry clone the
/// body instead of re-reading or re-copying it.
#[derive(Clone)]
pub(crate) enum UploadBody {
    Path(PathBuf),
    Bytes(Bytes),
}

/// Hashes exactly `part_size` bytes read from `file` (its current position)
/// into one MD5 digest -- a part's own digest, for the multipart-ETag
/// recipe below, or the whole file's digest when called once for a
/// small/empty file.
fn hash_file_part(file: &mut File, part_size: u64) -> Result<md5::Digest, String> {
    let mut context = md5::Context::new();
    let mut buf = vec![0u8; HASH_CHUNK_BYTES];
    let mut remaining = part_size;
    while remaining > 0 {
        let to_read = remaining.min(HASH_CHUNK_BYTES as u64) as usize;
        file.read_exact(&mut buf[..to_read])
            .map_err(|err| format!("failed to read file while hashing: {err}"))?;
        context.consume(&buf[..to_read]);
        remaining -= to_read as u64;
    }
    Ok(context.finalize())
}

/// The ETag S3 will report for `path` once uploaded with `UPLOAD_PART_SIZE`-
/// sized parts, alongside the file's size (so the caller doesn't need a
/// second `stat`): a plain hex MD5 for a file at or under that size (S3
/// never multiparts something that small -- matches a plain `put_object`'s
/// ETag), or the standard S3 multipart ETag --
/// `hex(md5(concat(part digests)))-<part count>` -- for anything larger.
/// Streams the file in `HASH_CHUNK_BYTES` chunks throughout, so this costs a
/// small, fixed amount of memory regardless of the file's size (ADR-0089).
///
/// Deliberately never calls `minio`'s own `calc_part_info` to double-check
/// this math: that function rejects an explicit, known part size whenever
/// `object_size / part_size` rounds up to a part count of 0 (a zero-byte
/// file) with `InvalidPartCount` -- a real landmine confirmed by reading
/// `minio` 0.4.0's source, not a hypothetical. This function's own
/// at-or-under-the-threshold branch (covering size 0 too) sidesteps it by
/// construction, and `upload_if_changed` only ever passes an explicit
/// `.part_size(...)` to the client when this function says multipart is
/// actually needed.
fn expected_etag_for_file(path: &Path) -> Result<(String, u64), String> {
    expected_etag_with_part_size(path, UPLOAD_PART_SIZE)
}

/// `expected_etag_for_file`'s actual logic, with the part size as a
/// parameter so a unit test can exercise the multipart branch against a
/// tiny fixture instead of a real `UPLOAD_PART_SIZE`-plus-sized file.
fn expected_etag_with_part_size(path: &Path, part_size: u64) -> Result<(String, u64), String> {
    let mut file =
        File::open(path).map_err(|err| format!("failed to open {}: {err}", path.display()))?;
    let size = file
        .metadata()
        .map_err(|err| format!("failed to stat {}: {err}", path.display()))?
        .len();

    if size <= part_size {
        let digest = hash_file_part(&mut file, size)?;
        return Ok((format!("{digest:x}"), size));
    }

    let part_count = size.div_ceil(part_size);
    let mut concatenated = Vec::with_capacity(part_count as usize * 16);
    let mut remaining = size;
    while remaining > 0 {
        let this_part = remaining.min(part_size);
        let digest = hash_file_part(&mut file, this_part)?;
        concatenated.extend_from_slice(&digest.0);
        remaining -= this_part;
    }
    let final_digest = md5::compute(&concatenated);
    Ok((format!("{final_digest:x}-{part_count}"), size))
}

fn build_client(bucket_config: &BucketConfig, secret_key: &str) -> Result<MinioClient, String> {
    let base_url: BaseUrl = bucket_config
        .endpoint
        .parse()
        .map_err(|err| format!("invalid endpoint '{}': {err}", bucket_config.endpoint))?;
    let provider = StaticProvider::new(&bucket_config.access_key_id, secret_key, None);
    MinioClient::new(base_url, Some(provider), None, None).map_err(|err| {
        format!(
            "failed to create S3 client for '{}': {err}",
            bucket_config.alias
        )
    })
}

/// Formats an S3 client error concisely. The `minio` crate's own `Display`
/// for a server-returned S3 error (`Error::S3Server(S3ServerError::S3Error)`)
/// is a verbose multi-line struct dump (code, message, resource, request
/// ID, host ID, bucket, object) meant for debugging, not a CLI's stderr --
/// this pulls just the code and message out of it instead. Every other
/// error variant's own `Display` is already a single line and is used as-is.
fn format_error(err: &S3Error) -> String {
    if let S3Error::S3Server(S3ServerError::S3Error(response)) = err {
        let code = response.code();
        return match response.message() {
            Some(message) => format!("{code:?}: {message}"),
            None => format!("{code:?}"),
        };
    }
    err.to_string()
}

/// Lists every bucket reachable with `bucket_config`'s credentials. No
/// longer backs a standalone CLI command (ADR-0017 removed `list-buckets`,
/// and ADR-0010 already removed `configure`'s inline discovery step before
/// that) -- kept as a plain function for reuse.
pub async fn list_buckets(
    bucket_config: &BucketConfig,
    secret_key: &str,
) -> Result<Vec<String>, String> {
    let client = build_client(bucket_config, secret_key)?;
    let resp = client
        .list_buckets()
        .build()
        .send()
        .await
        .map_err(|err| format!("failed to list buckets: {}", format_error(&err)))?;
    let buckets = resp
        .buckets()
        .map_err(|err| format!("failed to parse bucket list: {err}"))?;
    Ok(buckets
        .into_iter()
        .map(|bucket| bucket.name.to_string())
        .collect())
}

/// Checks whether `bucket_config.bucket` exists and is reachable with
/// `bucket_config`'s credentials. Used by `bucket-config new`/`edit` to
/// verify before persisting (ADR-0010) -- a bucket-scoped key that can't
/// call account-level `ListBuckets` should still be able to answer this.
pub(crate) async fn bucket_exists(
    bucket_config: &BucketConfig,
    secret_key: &str,
) -> Result<bool, String> {
    let client = build_client(bucket_config, secret_key)?;
    let resp = client
        .bucket_exists(bucket_config.bucket.as_str())
        .map_err(|err| format!("invalid bucket name '{}': {err}", bucket_config.bucket))?
        .build()
        .send()
        .await
        .map_err(|err| format!("failed to check bucket: {}", format_error(&err)))?;
    Ok(resp.exists())
}

/// Lists objects under `prefix` in `bucket_config`'s bucket. `recursive`
/// mirrors `ls` (flat, every object); non-recursive mirrors `lsd` (one level
/// of `/`-delimited pseudo-directories, `ObjectEntry::is_prefix` marking
/// them).
pub async fn list_objects(
    bucket_config: &BucketConfig,
    secret_key: &str,
    prefix: &str,
    recursive: bool,
) -> Result<Vec<ObjectEntry>, String> {
    let client = build_client(bucket_config, secret_key)?;
    let prefix = Some(prefix.to_string());
    let list = if recursive {
        client
            .list_objects(bucket_config.bucket.as_str())
            .map_err(|err| format!("invalid bucket name '{}': {err}", bucket_config.bucket))?
            .prefix(prefix)
            .recursive(true)
            .build()
    } else {
        client
            .list_objects(bucket_config.bucket.as_str())
            .map_err(|err| format!("invalid bucket name '{}': {err}", bucket_config.bucket))?
            .prefix(prefix)
            .delimiter(Some("/".to_string()))
            .build()
    };

    let mut stream = list.to_stream().await;
    let mut entries = Vec::new();
    while let Some(page) = stream.next().await {
        let page = page.map_err(|err| format!("failed to list objects: {}", format_error(&err)))?;
        for item in page.contents {
            entries.push(ObjectEntry {
                key: item.name,
                size: item.size.unwrap_or(0),
                is_prefix: item.is_prefix,
            });
        }
    }
    Ok(entries)
}

/// Downloads `key` from `bucket_config`'s bucket straight to `dest_path`,
/// streaming the response body in chunks the whole way (ADR-0076) -- never
/// buffers the whole object in memory, so a 50-100GB object costs a small,
/// fixed amount of RAM regardless of its size. `ObjectContent::to_file` (the
/// `minio` crate's own streaming helper, not a hand-rolled one) also
/// verifies the response's checksum incrementally per chunk and creates
/// `dest_path`'s parent directory if needed. Returns the number of bytes
/// written.
pub async fn download_object_to_file(
    bucket_config: &BucketConfig,
    secret_key: &str,
    key: &str,
    dest_path: &Path,
) -> Result<u64, String> {
    let client = build_client(bucket_config, secret_key)?;
    let resp = client
        .get_object(bucket_config.bucket.as_str(), key)
        .map_err(|err| format!("invalid object key '{key}': {err}"))?
        .build()
        .send()
        .await
        .map_err(|err| format!("failed to download '{key}': {}", format_error(&err)))?;
    resp.content()
        .map_err(|err| format!("failed to read '{key}': {err}"))?
        .to_file(dest_path)
        .await
        .map_err(|err| {
            format!(
                "failed to download '{key}' to {}: {err}",
                dest_path.display()
            )
        })
}

/// Uploads `body` to `key` in `bucket_config`'s bucket, but only if it
/// differs from what's already there. For a simple (non-multipart) PUT, an
/// S3 ETag is the hex MD5 digest of the object's bytes; for a multipart
/// upload, it's `hex(md5(concat(part digests)))-<part count>` -- so an
/// existing object's ETag (fetched via a HEAD request, `stat_object`, no
/// download needed) is compared directly against `body`'s locally
/// recomputed equivalent (`expected_etag_for_file` for a `Path`, a direct
/// `md5::compute` for an already-in-memory `Bytes`):
/// - no existing object: upload, `Uploaded`.
/// - existing object, matching hash: skip the PUT, `Unchanged`.
/// - existing object, different hash: note that the key changed (the bucket's
///   versioning means nothing is destroyed, but pigeon says so rather than
///   silently overwriting a stable-looking key), then upload, `Uploaded`.
///
/// A `Path` body is streamed the whole way through (ADR-0089): hashing runs
/// in `tokio::task::spawn_blocking` (CPU-bound, same ADR-0088 precedent --
/// shouldn't tie up a runtime worker thread for a multi-GB file), and the
/// upload itself goes through `put_object_content`, which streams the file
/// via `minio`'s own non-blocking async file reader and multipart-uploads
/// automatically above `UPLOAD_PART_SIZE` -- no 5GB single-PUT cap, and no
/// full-file buffer ever held in memory.
pub(crate) async fn upload_if_changed(
    bucket_config: &BucketConfig,
    secret_key: &str,
    key: &str,
    body: UploadBody,
) -> Result<UploadOutcome, String> {
    let client = build_client(bucket_config, secret_key)?;

    let existing_etag = match client
        .stat_object(bucket_config.bucket.as_str(), key)
        .map_err(|err| format!("invalid object key '{key}': {err}"))?
        .build()
        .send()
        .await
    {
        Ok(resp) => Some(
            resp.etag()
                .map_err(|err| format!("failed to read ETag for '{key}': {err}"))?
                .to_string(),
        ),
        Err(S3Error::S3Server(S3ServerError::S3Error(ref response)))
            if response.code() == MinioErrorCode::NoSuchKey =>
        {
            None
        }
        Err(err) => {
            return Err(format!("failed to check '{key}': {}", format_error(&err)));
        }
    };

    let (local_hash, size) = match &body {
        UploadBody::Path(path) => {
            let path = path.clone();
            tokio::task::spawn_blocking(move || expected_etag_for_file(&path))
                .await
                .map_err(|err| format!("hash task panicked: {err}"))??
        }
        UploadBody::Bytes(bytes) => (
            format!("{:x}", md5::compute(bytes.as_ref())),
            bytes.len() as u64,
        ),
    };

    if let Some(existing) = existing_etag {
        if existing == local_hash {
            return Ok(UploadOutcome::Unchanged);
        }
        println!("note: '{key}' changed since last upload, new version created");
    }

    match body {
        UploadBody::Path(path) => {
            let content = ObjectContent::from(path.as_path());
            let builder = client
                .put_object_content(bucket_config.bucket.as_str(), key, content)
                .map_err(|err| format!("invalid object key '{key}': {err}"))?;
            // Only set an explicit part size when the file actually needs
            // multipart -- see `expected_etag_for_file`'s doc comment for
            // why passing it unconditionally (including for a zero-byte
            // file) trips a `minio` 0.4.0 `InvalidPartCount` error.
            let send_result = if size > UPLOAD_PART_SIZE {
                builder.part_size(UPLOAD_PART_SIZE).build().send().await
            } else {
                builder.build().send().await
            };
            send_result
                .map_err(|err| format!("failed to upload '{key}': {}", format_error(&err)))?;
        }
        UploadBody::Bytes(bytes) => {
            let segmented = SegmentedBytes::from(bytes);
            client
                .put_object(bucket_config.bucket.as_str(), key, segmented)
                .map_err(|err| format!("invalid object key '{key}': {err}"))?
                .build()
                .send()
                .await
                .map_err(|err| format!("failed to upload '{key}': {}", format_error(&err)))?;
        }
    }
    Ok(UploadOutcome::Uploaded)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn write_fixture(bytes: &[u8]) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("fixture.bin");
        let mut file = File::create(&path).unwrap();
        file.write_all(bytes).unwrap();
        (dir, path)
    }

    #[test]
    fn streamed_md5_equals_md5_compute_on_a_small_fixture() {
        let data = b"the quick brown fox jumps over the lazy dog";
        let (_dir, path) = write_fixture(data);

        let (etag, size) = expected_etag_with_part_size(&path, UPLOAD_PART_SIZE).unwrap();

        assert_eq!(size, data.len() as u64);
        assert_eq!(etag, format!("{:x}", md5::compute(data)));
    }

    #[test]
    fn streamed_md5_handles_an_empty_file() {
        let (_dir, path) = write_fixture(b"");

        let (etag, size) = expected_etag_with_part_size(&path, UPLOAD_PART_SIZE).unwrap();

        assert_eq!(size, 0);
        assert_eq!(etag, format!("{:x}", md5::compute(b"")));
    }

    #[test]
    fn multipart_etag_matches_the_standard_s3_recipe() {
        // 25 deterministic bytes split into 10-byte parts (3 parts: 10, 10,
        // 5) -- expected value hand-computed via the same
        // hex(md5(concat(part_md5_digests)))-<part_count> recipe S3 itself
        // uses, independent of this crate's implementation.
        let data: Vec<u8> = (0u8..25).collect();
        let (_dir, path) = write_fixture(&data);

        let (etag, size) = expected_etag_with_part_size(&path, 10).unwrap();

        assert_eq!(size, 25);
        assert_eq!(etag, "704bbbf0caffa731e3361851ec17ae6c-3");
    }

    #[test]
    fn a_file_at_exactly_the_part_size_stays_single_part() {
        let data = vec![7u8; 10];
        let (_dir, path) = write_fixture(&data);

        let (etag, _size) = expected_etag_with_part_size(&path, 10).unwrap();

        assert_eq!(etag, format!("{:x}", md5::compute(&data)));
        assert!(!etag.contains('-'));
    }
}
