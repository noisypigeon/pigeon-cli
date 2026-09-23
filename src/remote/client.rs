use bytes::Bytes;
use futures::StreamExt;
use minio::s3::MinioClient;
use minio::s3::creds::StaticProvider;
use minio::s3::error::{Error as S3Error, S3ServerError};
use minio::s3::http::BaseUrl;
use minio::s3::minio_error_response::MinioErrorCode;
use minio::s3::response_traits::HasEtagFromHeaders;
use minio::s3::segmented_bytes::SegmentedBytes;
use minio::s3::types::{S3Api, ToStream};

use crate::remote::store::Remote;

/// A listed S3 object or (in non-recursive/`lsd` mode) common prefix.
pub(crate) struct ObjectEntry {
    pub key: String,
    pub size: u64,
    pub is_prefix: bool,
}

/// Outcome of `upload_if_changed`'s existing-object hash comparison.
pub(crate) enum UploadOutcome {
    Uploaded,
    Unchanged,
}

fn runtime() -> Result<tokio::runtime::Runtime, String> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|err| format!("failed to start async runtime: {err}"))
}

fn build_client(remote: &Remote, secret_key: &str) -> Result<MinioClient, String> {
    let base_url: BaseUrl = remote
        .endpoint
        .parse()
        .map_err(|err| format!("invalid endpoint '{}': {err}", remote.endpoint))?;
    let provider = StaticProvider::new(&remote.access_key_id, secret_key, None);
    MinioClient::new(base_url, Some(provider), None, None)
        .map_err(|err| format!("failed to create S3 client for '{}': {err}", remote.alias))
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

/// Lists every bucket reachable with `remote`'s credentials (ADR-0009's
/// `list-buckets`).
pub(crate) fn list_buckets(remote: &Remote, secret_key: &str) -> Result<Vec<String>, String> {
    runtime()?.block_on(async {
        let client = build_client(remote, secret_key)?;
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
    })
}

/// Checks whether `remote.bucket` exists and is reachable with `remote`'s
/// credentials. Used by `configure`/`edit` to verify before persisting
/// (ADR-0010) -- a bucket-scoped key that can't call account-level
/// `ListBuckets` should still be able to answer this.
pub(crate) fn bucket_exists(remote: &Remote, secret_key: &str) -> Result<bool, String> {
    runtime()?.block_on(async {
        let client = build_client(remote, secret_key)?;
        let resp = client
            .bucket_exists(remote.bucket.as_str())
            .map_err(|err| format!("invalid bucket name '{}': {err}", remote.bucket))?
            .build()
            .send()
            .await
            .map_err(|err| format!("failed to check bucket: {}", format_error(&err)))?;
        Ok(resp.exists())
    })
}

/// Lists objects under `prefix` in `remote`'s bucket. `recursive` mirrors
/// `ls` (flat, every object); non-recursive mirrors `lsd` (one level of
/// `/`-delimited pseudo-directories, `ObjectEntry::is_prefix` marking them).
pub(crate) fn list_objects(
    remote: &Remote,
    secret_key: &str,
    prefix: &str,
    recursive: bool,
) -> Result<Vec<ObjectEntry>, String> {
    runtime()?.block_on(async {
        let client = build_client(remote, secret_key)?;
        let prefix = Some(prefix.to_string());
        let list = if recursive {
            client
                .list_objects(remote.bucket.as_str())
                .map_err(|err| format!("invalid bucket name '{}': {err}", remote.bucket))?
                .prefix(prefix)
                .recursive(true)
                .build()
        } else {
            client
                .list_objects(remote.bucket.as_str())
                .map_err(|err| format!("invalid bucket name '{}': {err}", remote.bucket))?
                .prefix(prefix)
                .delimiter(Some("/".to_string()))
                .build()
        };

        let mut stream = list.to_stream().await;
        let mut entries = Vec::new();
        while let Some(page) = stream.next().await {
            let page =
                page.map_err(|err| format!("failed to list objects: {}", format_error(&err)))?;
            for item in page.contents {
                entries.push(ObjectEntry {
                    key: item.name,
                    size: item.size.unwrap_or(0),
                    is_prefix: item.is_prefix,
                });
            }
        }
        Ok(entries)
    })
}

/// Downloads `key` from `remote`'s bucket into memory. Whole-object,
/// non-streaming -- fine for the email-archive-sized files this project
/// deals with; chunked/multipart transfer is future work if that changes.
pub(crate) fn get_object(remote: &Remote, secret_key: &str, key: &str) -> Result<Vec<u8>, String> {
    runtime()?.block_on(async {
        let client = build_client(remote, secret_key)?;
        let resp = client
            .get_object(remote.bucket.as_str(), key)
            .map_err(|err| format!("invalid object key '{key}': {err}"))?
            .build()
            .send()
            .await
            .map_err(|err| format!("failed to download '{key}': {}", format_error(&err)))?;
        let content = resp
            .content()
            .map_err(|err| format!("failed to read '{key}': {err}"))?
            .to_segmented_bytes()
            .await
            .map_err(|err| format!("failed to read '{key}': {err}"))?;
        Ok(content.to_bytes().to_vec())
    })
}

/// Uploads `data` to `key` in `remote`'s bucket, but only if it differs from
/// what's already there. For a simple (non-multipart) PUT, an S3 ETag is the
/// hex MD5 digest of the object's bytes -- so an existing object's ETag
/// (fetched via a HEAD request, `stat_object`, no download needed) is
/// compared directly against `data`'s local hex MD5:
/// - no existing object: upload, `Uploaded`.
/// - existing object, matching hash: skip the PUT, `Unchanged`.
/// - existing object, different hash: note that the key changed (the bucket's
///   versioning means nothing is destroyed, but pigeon says so rather than
///   silently overwriting a stable-looking key), then upload, `Uploaded`.
pub(crate) fn upload_if_changed(
    remote: &Remote,
    secret_key: &str,
    key: &str,
    data: Vec<u8>,
) -> Result<UploadOutcome, String> {
    runtime()?.block_on(async {
        let client = build_client(remote, secret_key)?;

        let existing_etag = match client
            .stat_object(remote.bucket.as_str(), key)
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

        let local_hash = format!("{:x}", md5::compute(&data));

        if let Some(existing) = existing_etag {
            if existing == local_hash {
                return Ok(UploadOutcome::Unchanged);
            }
            println!("note: '{key}' changed since last upload, new version created");
        }

        let bytes = SegmentedBytes::from(Bytes::from(data));
        client
            .put_object(remote.bucket.as_str(), key, bytes)
            .map_err(|err| format!("invalid object key '{key}': {err}"))?
            .build()
            .send()
            .await
            .map_err(|err| format!("failed to upload '{key}': {}", format_error(&err)))?;
        Ok(UploadOutcome::Uploaded)
    })
}
