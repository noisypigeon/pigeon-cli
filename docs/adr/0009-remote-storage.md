# ADR-0009: `pigeon remote` — rclone-style S3-compatible remote storage

- **Author**: Willow Finch ([@noisypigeon](https://github.com/noisypigeon)).
- **Date**: 2026-09-22.
- **Status**: Accepted.

## Context

`pigeon` gains its first command group beyond `email`: `pigeon remote`, for pushing/pulling data to and from S3-compatible object storage (DigitalOcean Spaces is the motivating example, explicitly S3-compliant), behaving like rclone. This is exactly the second command group ADR-0008 restructured `service/pigeon-cli/src/` to accommodate. This ADR decides the S3 client crate, the command surface (`configure`, `list-buckets`, `ls`, `lsd`, `copy`), the remote-addressing model, and config/secret storage.

## Decision

### S3-compatible client crate: `minio`

Three real contenders were compared:

- **`object_store`** (Apache Arrow ecosystem, donated by InfluxData; powers DataFusion, Delta Lake, Iceberg, Polars/Lance) — extremely reputable via adoption, but **structurally can't do `list-buckets`**: every backend (`AmazonS3Builder`, etc.) is constructed with a bucket name up front — it's a bucket-scoped object abstraction, not an account-level S3 client. Since `list-buckets` is an explicit required subcommand, this rules it out as the sole dependency.
- **`rust-s3`** — purpose-built for "Amazon S3 or arbitrary S3 compatible APIs" and does support bucket listing, but its own maintainer describes attention to it as "oscillating" and solicits donations to keep it maintained — a real flag against wanting something reputable.
- **`minio`** (crates.io: `minio`, v0.4.0, Apache-2.0, published by the MinIO organization) — the official MinIO Rust SDK, documented for "any Amazon S3 compatible object storage service," not just MinIO itself. Confirmed support for listing all buckets, listing objects/directories with prefix filtering, and upload/download, via a fluent builder API (`S3Api` trait, async `send()`). A client is constructed from an arbitrary `BaseUrl` (any host) plus a `StaticProvider` for access/secret key — a direct fit for DigitalOcean Spaces' custom-endpoint model. Backed by a real company whose core business is S3-compatible storage.

**Decision: `minio`.** `aws-sdk-s3` (the official AWS SDK) is also fully capable and was considered — it's noted as the alternative for anyone who specifically wants AWS's own name behind the dependency, at the cost of a much heavier transitive dependency tree and AWS-centric framing for a tool that's explicitly targeting S3-*compatible* (not AWS) storage first.

### Bucket-scoped remote model

Each configured remote is **one endpoint + one bucket + one credential set** — not rclone's more general model, where a single remote can browse many differently-named buckets. This is grounded directly in the feature request: `configure` collects "bucket URL, access key, secret key" as one unit, and every `ls`/`lsd`/`copy` example addresses a bare `name:` with nothing typed after the colon. A bucket-scoped model is simpler than rclone proper, sufficient for this project's actual need (a single designated backup destination for the transformed email archive), and still supports `name:sub/path` addressing within that one bucket.

### Command surface

```
pigeon remote configure [NAME]
    -- NAME optional (prompted as text input if omitted -- it's a new remote being created)
    -- interactive: endpoint URL, access key ID, secret access key (masked/TTY-aware, reusing
       email::commands::read_secret's dual TTY/piped-stdin pattern), optional region; then an
       inline "list buckets with these credentials?" step to help pick, then bucket name.

pigeon remote list-buckets [NAME]
    -- NAME optional, interactive select among configured remotes when omitted
       (reusing email::identity::Store::prompt_select's pattern)
    -- prints bucket names reachable with that remote's stored credentials

pigeon remote lsd NAME:[PATH]
    -- non-recursive: S3 prefix+delimiter listing -- the standard "S3-as-filesystem" pseudo-directory
       convention, exactly what rclone itself does under the hood for S3 backends too

pigeon remote ls NAME:[PATH]
    -- recursive flat listing: size + path per object, no delimiter

pigeon remote copy SOURCE DEST
    -- each of SOURCE/DEST is a bare filesystem path or `name:path` (colon-triggered remote parse
       against configured remote names); local<->remote in either direction, matching rclone's own
       copy exactly. Remote-to-remote (two different configured remotes) is out of scope for v1.
```

### Resolving the `list-buckets`-before-a-bucket-exists chicken-and-egg

`configure` needs a bucket name to save a complete remote, but discovering valid bucket names needs credentials first — and `list-buckets` as a subcommand needs an already-configured remote to reuse credentials from. Resolved by not conflating the two: `configure`'s interactive flow collects endpoint + access key + secret key first, then **inline** (not via a separate CLI invocation) offers to call the same underlying list-buckets logic to show available bucket names for picking, then asks for/confirms the bucket, then saves everything. The standalone `pigeon remote list-buckets [NAME]` subcommand is for later, post-setup use (auditing, re-discovery) against an already-configured remote's stored credentials — both paths share one underlying function.

### Config and secret storage

`remotes.toml` reuses the exact pattern `email::identity::Store` already established: `directories::ProjectDirs` + `PIGEON_CONFIG_DIR` env override + the `toml` crate (already a dependency) — a separate file from `identities.toml`, holding only non-secret metadata per remote: name, endpoint, bucket, access key ID, region.

The secret access key goes in the OS keychain via `keyring`, matching ADR-0003's precedent for IMAP app passwords exactly — never written to the TOML file. Consistent security posture across the whole CLI, not a special case for this feature.

### `service/pigeon-cli/src/remote/` — reusing ADR-0008's shape

Mirrors `service/pigeon-cli/src/email/`'s structure exactly: `mod.rs`, `cli.rs`, `commands.rs`, plus logic modules (an S3 client wrapper, a `Remote`/`Store` pair analogous to `email::identity::Identity`/`Store`, a `remote:path` parser). One new `Commands::Remote(RemoteArgs)` variant in `service/pigeon-cli/src/cli.rs`, one new match arm in `service/pigeon-cli/src/commands/mod.rs` — exactly the extension path ADR-0008's Consequences described.

## Consequences

- `pigeon remote` becomes implementable: configure, discover buckets, list, and copy against any S3-compatible endpoint, with the CLI's existing config/secret-storage conventions reused rather than reinvented.
- This is deliberately a *subset* of rclone's generality — bucket-scoped remotes, S3-compatible storage only, no `sync`/`move`/`delete`-style destructive remote operations, no remote-to-remote copy. Anyone expecting full rclone parity should know that going in; the scope boundary is intentional, not an oversight, and each item is a plausible future ADR rather than a closed door.
- `service/pigeon-cli/src/remote/` becomes the second proof point (after `service/pigeon-cli/src/email/`) that ADR-0008's per-command-group folder shape actually scales to a genuinely different domain, not just a hypothetical.

## Out of scope

- Non-S3 backends (GCS, Azure, local-to-local sync) — S3-compatible only, per the feature request. ([#11](https://github.com/noisypigeon/pigeon-cli/issues/11))
- `sync`, `move`, or `delete`-style operations that can remove data on the remote — `copy` only, deliberately non-destructive on both ends. ([#12](https://github.com/noisypigeon/pigeon-cli/issues/12))
- Multi-bucket-per-remote browsing (rclone's fuller generality) — one remote, one bucket, per the bucket-scoped model above. ([#13](https://github.com/noisypigeon/pigeon-cli/issues/13))
- Remote-to-remote `copy` (two different configured remotes as SOURCE and DEST). ([#14](https://github.com/noisypigeon/pigeon-cli/issues/14))
