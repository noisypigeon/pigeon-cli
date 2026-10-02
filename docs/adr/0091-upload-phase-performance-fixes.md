# ADR-0091: upload-phase performance fixes

- **Author**: Willow Finch ([@noisypigeon](https://github.com/noisypigeon)).
- **Date**: 2026-10-02.
- **Status**: Accepted.

## Context

A real `job run dedupe` run (560,705 kept files, 216 GiB) was log-analyzed
end to end after recovering from the OOM kill ADR-0089/ADR-0090 already
fixed. The dedup/content-hashing logic itself checked out with very high
confidence: every one of the 560,705 kept files, re-hashed from disk,
matched its recorded SHA-256; every one of 3,025,004 duplicate records in
the merge report pointed at a hash genuinely present in the index; the
index, result tree, and upload checkpoint each held exactly 560,705 entries
with no hash or path repeated. Separately, 53 nested zips (51
password-protected, 2 corrupt) failed to open and were silently dropped
without being duplicates of anything — the user is deciding what behavior
change (if any) that deserves on its own, in a separate ADR; it is **out of
scope here**.

What *is* in scope: three concrete operational/performance problems the
run's successful second half (upload-only, after the OOM fix) exposed.

**1. Every file is read from disk twice.** `upload_if_changed`
(`src/commands/keyring/bucket/client.rs:303`, introduced by ADR-0089)
unconditionally streams the whole file through MD5 to compute a comparison
hash (`expected_etag_for_file`, lines 98-129, via `spawn_blocking`) *before*
checking whether that comparison is even useful — then, if the upload
proceeds, the `minio` crate's `ObjectContent::from(path)` (via
`put_object_content`) streams the same file a second time for the actual
PUT/multipart body. The real run read 282 GiB off disk to upload 194 GiB.
The local hash only matters when an object already exists at the
destination key (`existing_etag.is_some()`); for a fresh destination — the
common case for a `dedupe`/`sort`/`pull-transform` run into a new or mostly-
empty bucket — it's a fully wasted read, costing roughly another file's
worth of I/O per upload for no benefit.

**2. No client-side upload timeout exists anywhere.** Three tiny `.md`
files each hung for exactly 1800s before completing; the first (OOM-killed)
run separately showed the same pattern twice. `build_client`
(`client.rs:131-143`) calls `MinioClient::new(base_url, Some(provider),
None, None)` — the `minio` 0.4.0 crate's convenience constructor, confirmed
by reading its vendored source to build a `reqwest::Client` configured only
with connection-pool settings (`tcp_nodelay`, `tcp_keepalive`,
`pool_max_idle_per_host`, a 90s `pool_idle_timeout`) and no
`.timeout(...)`/`.connect_timeout(...)` call at all. `reqwest::ClientBuilder`
itself defaults to no per-request timeout absent an explicit one. A stalled
connection can therefore occupy one of a job's limited upload concurrency
slots indefinitely; `retry_with_backoff` (`src/core/retry.rs:12-43`,
already wrapping the call at `upload.rs::upload_one:208-211` per ADR-0024)
never gets a chance to retry, because nothing ever returns an error for it
to catch.

**3. Upload concurrency piggybacks on unrelated concurrency.** Every job
that uploads (`dedupe`, `sort`, `pull-transform`, `email-sync`,
`email-pull`) passes the exact same `concurrency` value into both its
primary work (download/hash/transform for the first three; IMAP mailbox
fetch for the latter two) *and* `run_upload_phase`
(`src/commands/job/upload.rs:250`, `stream::buffer_unordered(concurrency)`
at line 276). The real run's upload phase was bottlenecked by per-file
round-trip latency, not bytes or CPU: median 0.17s/file, p99 1.2s, only 8
uploads in flight throughout, ~30 files/sec on the three hours of small
files that preceded the last 20 minutes' large-file burst. A
network-round-trip-bound phase has no principled reason to be capped at
core count (`CpuConcurrencyInput`, used by `dedupe`/`pull-transform`) or a
flat value tuned for IMAP (`ConcurrencyInput`, used by `sort`/`email-sync`/
`email-pull`) — both are the wrong dial for it, in either direction.

Per an explicit scoping decision, fix 3 generalizes to all 5 upload-capable
job types in this one ADR rather than `dedupe`-only, matching ADR-0090's own
precedent of auditing a dedupe-born fix and generalizing it across every job
sharing the code.

## Decision

### 1. Skip the wasted hash when there's nothing to compare against

In `client.rs`, `upload_if_changed` is reordered so the expensive hash is
computed lazily, only when it can actually change the outcome:

- `fn needs_hash(existing_etag: &Option<String>, body: &UploadBody) -> bool`
  — `true` for any `Bytes` body (already free: an in-memory buffer that's
  there for encryption anyway) or whenever `existing_etag.is_some()`.
- `fn file_size(path: &Path) -> Result<u64, String>` — a metadata-only
  `stat`, no file open, no read at all.
- The former unconditional hash block (lines 333-344) becomes conditional on
  `needs_hash`: when false for a `Path` body, call `file_size` instead of
  `expected_etag_for_file`, yielding `local_hash: None` alongside the cheaply
  obtained `size`; when true, behave exactly as before, yielding
  `local_hash: Some(hash)`.
- The comparison block (lines 346-351) is unchanged in effect: it only runs
  when `existing_etag.is_some()`, which by `needs_hash`'s construction means
  `local_hash` is always `Some` there too — unwrapped with a documenting
  `.expect(...)` describing that invariant, not a new fallible path.
- The upload dispatch (lines 353-382) is untouched: it only ever needed
  `size` (for the `size > UPLOAD_PART_SIZE` multipart decision), which is
  populated identically either way.

No change to `UploadBody`, `expected_etag_for_file`,
`expected_etag_with_part_size`, or `hash_file_part` — only *when*
`upload_if_changed` calls into the hashing path changes, and only for the
`Path` branch (`Bytes` always hashes, since it's already in memory and
cheap).

### 2. A size-scaled per-attempt upload timeout

This lives in `upload.rs`, not `client.rs`/`build_client`: a flat
`reqwest::Client`-level timeout can't distinguish a genuinely large,
genuinely-progressing multipart upload (the real run's 30.7 GiB `.mov`
upload succeeded, taking a meaningful fraction of the last 20 minutes) from
a stalled small one, and `MinioClient::new`'s convenience constructor
doesn't expose the underlying builder to set one anyway.

New constants:

```rust
const UPLOAD_TIMEOUT_FLOOR: Duration = Duration::from_secs(60);
const UPLOAD_TIMEOUT_MIN_THROUGHPUT_BYTES_PER_SEC: u64 = 10 * 1024 * 1024; // 10 MiB/s
```

`fn upload_timeout(bytes: u64) -> Duration` returns
`UPLOAD_TIMEOUT_FLOOR + Duration::from_secs(bytes /
UPLOAD_TIMEOUT_MIN_THROUGHPUT_BYTES_PER_SEC)` — comfortably above the real
run's observed per-file latency (p99 1.2s) for small files, and a
deliberately pessimistic 10 MiB/s floor-throughput allowance for large ones
(roughly 53 minutes for a 30 GiB file) so a legitimate large transfer is
never mistaken for a stall.

`async fn with_upload_timeout<T>(duration: Duration, fut: impl Future<Output
= Result<T, String>>) -> Result<T, String>` wraps `tokio::time::timeout`,
flattening its `Result<Result<T, String>, Elapsed>` into the same
`Result<T, String>` shape every other failure in this path already uses — a
timeout becomes just another retryable error string, with no changes needed
to `client.rs` or `core/retry.rs`.

In `upload_one` (around line 208), the `retry_with_backoff` closure's inner
future is wrapped:

```rust
let timeout_duration = upload_timeout(bytes_for_log);
retry_with_backoff(UPLOAD_RETRIES, UPLOAD_RETRY_BACKOFF, || {
    with_upload_timeout(
        timeout_duration,
        client::upload_if_changed(bucket_config, secret, &task.key, body.clone()),
    )
})
.await
```

`bytes_for_log` (already computed via `fs::metadata` at line 185 for
tracing) is reused as-is — no new stat call. Sizing off the source file's
length is a reasonable proxy even for an encrypted (`Bytes`) upload, since
AES-256-GCM-SIV ciphertext is within a few bytes of plaintext length.

### 3. A dedicated, network-bound upload concurrency knob — all 5 jobs

A new shared `WizardInput` in `shared_wizard.rs`, alongside the existing
`ConcurrencyInput`/`CpuConcurrencyInput`:

```rust
const UPLOAD_CONCURRENCY_DEFAULT: usize = 16;

pub(crate) struct UploadConcurrencyInput { pub flag: Option<usize> }
```

Same `WizardInput` shape as its siblings (`flag_value`/`prompt` identical in
spirit), except `non_interactive_fallback` returns
`Ok(UPLOAD_CONCURRENCY_DEFAULT)` rather than erroring: unlike
`ConcurrencyInput`'s hard requirement, `--upload-concurrency` is a brand new
flag being added to commands that already run unattended in scripts/cron
today, and requiring it non-interactively would break every existing
invocation that predates this flag.

`core::job::Job::run` gains a second parameter:
`upload_concurrency: usize`. `DecryptFilesJob` (no upload phase) accepts and
ignores it (bound as `_upload_concurrency`) rather than gaining a new
`--upload-concurrency` flag — matching ADR-0090's "not uniformly" precedent
for concurrency-related changes that don't apply to every job.

Each of the 5 upload-capable `JobType` variants in `cli.rs` (`EmailSync`,
`EmailPull`, `Sort`, `Dedupe`, `PullTransform`) gains
`upload_concurrency: Option<usize>` next to its existing `concurrency`
field; `commands.rs` threads it through to each job's `wizard::dispatch`
unchanged, mirroring the existing `concurrency` plumbing exactly. Each
job's `dispatch_async` resolves a second value via `UploadConcurrencyInput`
(alongside its existing `CpuConcurrencyInput`/`ConcurrencyInput`
resolution) and calls `job.run(plan, concurrency, upload_concurrency)`.

Each job's `--upload-only` path (`dispatch_upload_only`, which has no
download/hash/transform/fetch phase at all to need the original concurrency
value for) **replaces** its existing concurrency resolution outright with
`UploadConcurrencyInput`, rather than adding a second one — see
Consequences for the resulting behavior change.

Each job's worker function (`run_dedupe_job`, `run_sort_job`,
`run_pull_transform_job`, `run_email_sync_job`, `run_email_pull_job`, and
each job's own `run_upload_only`) gains the new parameter, threading it to
*only* its `run_upload_phase`/`upload_result` call site. Every other use of
the original `concurrency` — sizing the download/hash/transform worker pool
for `dedupe`/`sort`/`pull-transform`, or the IMAP fetch worker pool plus
per-identity connection `Semaphore` for `email-sync`/`email-pull` — is
untouched.

## Consequences

- Uploading into a fresh destination (no existing object at that key) — the
  common case for a first `dedupe`/`sort`/`pull-transform` run — now reads
  each file once instead of twice.
- A stalled upload attempt now fails (and is retried by the existing
  3-attempt backoff, or ultimately counted as a logged upload failure)
  within roughly a minute to under an hour depending on file size, instead
  of being able to hang indefinitely and hold a concurrency slot for as long
  as whatever external layer eventually kills it.
- `--upload-concurrency` is a new, independently-tunable flag on `dedupe`,
  `sort`, `pull-transform`, `email-sync`, and `email-pull`, defaulting to
  16 — decoupled from `--concurrency`, which keeps its original meaning
  (sizing each job's primary, non-upload work) unchanged.
- **Behavior change**: for all 5 jobs' `--upload-only` resume path,
  `--concurrency` no longer has any effect on upload parallelism — only the
  new `--upload-concurrency` does, since that path has no other work for
  `--concurrency` to size. `--concurrency` is still accepted there (so a
  script invoking the old flag doesn't fail to parse) but is silently
  ignored.
- `DecryptFilesJob` does not gain an `--upload-concurrency` flag, since it
  never uploads — its `Job::run` impl takes and ignores the new trait
  parameter purely to satisfy the trait signature uniformly.

## Out of scope

- The 53 unopened nested zips (51 password-protected, 2 corrupt) that were
  silently dropped without being duplicates of anything found during the
  same log analysis. Whether `dedupe` should instead keep an unopenable zip
  as a plain file is a behavior change to ADR-0082, being decided and
  written up separately.

## Verification

- Unit tests: `needs_hash` returns `false` for a `Path` body with no
  existing etag and `true` once one exists or the body is `Bytes`; a
  `#[cfg(unix)]` permission-bits test proves `file_size` succeeds on a file
  whose read permission bit has been stripped (while `File::open` on the
  same path fails), confirming no read-open happens; `upload_timeout`
  returns the floor for small sizes and scales upward for a 30 GiB size;
  `with_upload_timeout` converts a `pending()` future into a `"timed out"`
  string error under a short timeout and passes a fast `Ok` straight
  through unchanged; `UploadConcurrencyInput`'s `flag_value` and
  `non_interactive_fallback` resolve as expected; a CLI test per job
  confirms `--upload-concurrency` parses on all 5 upload-capable `JobType`
  variants and is absent from `DecryptFiles`'s help output.
- `mise run ci` clean.
- Manual: re-run `job run dedupe --upload-only` against a local fixture tree
  with a mix of already-existing and new destination keys, confirming no
  behavior change in what gets uploaded/skipped; point at a deliberately
  unreachable bucket endpoint and confirm each file now fails within its
  computed timeout instead of hanging; confirm `--upload-concurrency 32`
  visibly changes the number of concurrent in-flight uploads independent of
  `--concurrency`.
