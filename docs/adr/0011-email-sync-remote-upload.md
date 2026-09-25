# ADR-0011: wire `--output-remote` into `pigeon email sync`

- **Author**: Willow Finch ([@noisypigeon](https://github.com/noisypigeon)).
- **Date**: 2026-09-22.
- **Status**: Accepted.

## Context

```
pigeon email sync [ALIAS] --staging-dir <dir> --output-dir <dir> --output-remote <remote> [--debug <sink|transform>]
```

The user wants `pigeon email sync` to optionally upload its `--output-dir` result to a configured `pigeon remote` (ADR-0009/0010). This is the first time `email` and `remote` — deliberately independent siblings per ADR-0008 — need to call into each other. It's also the first real case of the "extract a shared abstraction once a second concrete consumer exists" scenario ADR-0008's own text anticipated, except simpler than a full extraction: `email` becomes a consumer of a capability `remote::client` already exposes at crate visibility, not a new shared module. This ADR decides the CLI shape, when uploads happen, and — per the user's explicit ask — how to tell "the same file" from "a different file" at a colliding key in a versioned bucket.

## Decision

### `upload_if_changed`: answering "same file or different file" with data already in hand

`remote::client::list_objects` already returns `ListEntry { name, etag: Option<String>, size: Option<u64>, .. }` for existing objects — `etag` comes for free from a call `remote::client` already makes, no new S3 API surface needed. For a simple (non-multipart) PUT — exactly what `remote::client::put_object` does — an S3 ETag is the hex MD5 digest of the object's bytes, confirmed directly against the `minio` crate's own internal `md5sum_hash` helper (which computes `md5::compute(data)` for its `Content-MD5` header, just base64- instead of hex-encoded for that use). The `md5` crate — already resolved in `Cargo.lock` as a transitive dependency of `minio` itself — exposes `md5::compute(data) -> Digest`, and `Digest` implements `LowerHex`, giving the standard 32-character lowercase hex string directly comparable to `ListEntry.etag` once its surrounding quotes are stripped.

This grounds a new `remote::client::upload_if_changed(remote, secret_key, key, data) -> Result<UploadOutcome, String>`: look up the destination key's current ETag (if any), compute the local content's hex MD5, and:
- **No existing object**: upload, `UploadOutcome::Uploaded`.
- **Existing object, matching hash**: skip the PUT entirely, `UploadOutcome::Unchanged`.
- **Existing object, different hash**: this is the real "name collision" case the user asked about. Print a one-line notice (`note: '<key>' changed since last upload, new version created`) and upload anyway — the bucket's versioning means nothing is ever destroyed, but pigeon says so rather than staying silent about a change at a stable key. `UploadOutcome::Uploaded`.

`upload_if_changed` lives in `remote::client`, reusable by both the new `email sync` integration and `remote::commands::upload` (`remote copy`) — a free efficiency improvement for `remote copy` as a side effect of building this, not a second copy of the same logic.

### CLI: `--output-remote`, default-flow only

`--output-remote <alias>` names a configured remote (`remote::store::Store`). It's resolved and validated up front — the alias exists, its secret is retrievable from the keychain — before any IMAP connection is attempted, same "resolve everything before doing expensive work" pattern already used for `--alias` resolution.

**Both `--debug sink` and `--debug transform` reject `--output-remote` as a usage error.** Debug modes stay purely local/inspection-only, per ADR-0007's own framing of what `--debug` means — there's no partial exception carved out for `--debug transform` even though it does write to `--output-dir`; keeping the rule uniform ("`--debug` means local-only, full stop") is simpler than a rule with one exception.

### Upload timing: per-message, inline in the existing pipeline

Uploads happen per-message, inline in `sync::run_async`'s existing fetch → transform → verify → delete pipeline (ADR-0007) — not a bulk pass over `--output-dir` at the end. Right after `transform::transform_one` and `transform::verify_transformed` succeed for a message, and *before* its `.eml` is deleted and its UID marked `.processed`, that message's `.md` and any attachments are uploaded via `upload_if_changed`.

A failed upload means the message is **not** marked processed and its `.eml` is **not** deleted — the same verify-before-persist gate ADR-0007 established for local verification, extended to cover the remote copy too. This also makes re-running `sync --output-remote` naturally efficient without any separate remote-side resume bookkeeping: a message already marked `.processed` is never revisited by the pipeline at all, so it's never reconsidered for upload either — resume "for free," inherited from ADR-0007's existing mechanism rather than reimplemented.

### S3 key convention

Uploads mirror `--output-dir`'s relative tree directly at the configured remote's bucket root — identical to how `remote copy <local-dir> <alias>:` already lays things out (ADR-0009), no extra prefix. Remotes are already bucket-scoped, one remote per bucket, so there's no ambiguity about where "this identity's email archive" lives once a remote is chosen for the purpose.

## Consequences

- `email::commands` gains its first dependency on `remote::client`/`remote::store` — a real, justified cross-domain call now that there's a second concrete consumer of upload logic, exactly the case ADR-0008 left room for without prescribing in advance.
- `remote copy` gets unchanged-file skipping as a side effect of building `upload_if_changed` for this feature, not a separate effort.
- `sync`'s summary output gains upload counts (uploaded / unchanged / failed) alongside its existing message counts.
- A sync interrupted partway through leaves exactly the messages it finished (locally verified *and* uploaded) marked `.processed` — no separate "did this one make it to the remote" state to track or get out of sync with the existing `.processed` marker.

## Out of scope

- Deleting remote objects that no longer have a local counterpart — no sync/mirror semantics, matching ADR-0009's `copy`-only, never-destructive stance. ([#12](https://github.com/noisypigeon/pigeon-cli/issues/12))
- Uploading to more than one remote at once. ([#16](https://github.com/noisypigeon/pigeon-cli/issues/16))
- Any change to `remote`'s own standalone commands beyond adding the shared `upload_if_changed` primitive.
- Implementation itself — like every ADR before its own separate implementation request, this is a decision record only.
