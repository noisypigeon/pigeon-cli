# ADR-0136: sandbox rclone to a pigeon-keyring-generated config

- **Author**: Willow Graysen ([@noisypigeon](https://github.com/noisypigeon)).
- **Date**: 2026-10-12.
- **Status**: Accepted.

## Context

Every `rclone` invocation this crate makes must run against a config file
pigeon itself generates entirely from its own keyring -- never a
separately-provisioned deployment `rclone.conf`, never the operator's own
general-purpose one. `rclone copy`/`delete` (ADR-0101, restructured by
ADR-0110) and `transform` (ADR-0112) currently reach raw `remote:path`
strings against whatever config `rclone` finds ambiently (its default path,
or `RCLONE_CONFIG`); that ambient-trust model is exactly what this ADR
removes.

This explicitly reverses a decision made three times. ADR-0101 (§1, "Out of
scope"): *"Any translation of pigeon's own `BucketConfig`/keyring
credentials into rclone-usable config."* ADR-0132 and ADR-0133 each
reaffirmed it verbatim while separately proposing (never implemented) to
consolidate rclone invocations behind one trait. The reversal is justified
by two things confirmed directly:

1. **Every real use of `rclone` in this deployment has only ever addressed
   pigeon-managed buckets.** The "reach arbitrary non-S3 backends via an
   externally-provisioned config" capability ADR-0101 built `rclone
   copy`/`delete` for has no evidenced use.
2. **The translation this ADR performs already happens today, just outside
   pigeon.** `noisypigeon/pigeon.dev`'s terraform cloud-init template
   (`templates/terraform/scaleway/compute-instance-config/config.tf`)
   writes `/root/.config/rclone/rclone.conf` directly from the same keyring
   data pigeon itself manages, with each bucket-kind entry's alias as the
   rclone remote name. Pigeon generating this file itself, instead of
   trusting a file some other process wrote, removes a dependency on a
   process pigeon cannot see or test, and makes alias-addressed rclone
   usage work on a plain dev machine with no hand-provisioning at all.

Folded into this same change, since neither had landed any code yet
(`docs/adr/0132-*.md`/`0133-*.md` were both `Status: Proposed`, no
`src/core/rclone/` existed): ADR-0132's `RcloneCommand` trait consolidation
(six hand-written invocations, each re-deriving the same subprocess
scaffolding) and ADR-0133's migration of `deduplicate`'s data legs off
`minio`. Building the sandboxed-config design on top of the raw-path shape
those two ADRs described, then reversing part of it in a second pass,
would have been strictly more work for no benefit -- so this ADR documents
the final shape directly and both are implemented together with it.

## Decision

### 1. `src/core/rclone/`: one trait, four implementors, one runner (ADR-0132)

```
src/core/rclone/mod.rs      RcloneCommand trait, Observation/Labels/Outcome, execute(), check_available()
src/core/rclone/command.rs  Copy / Purge / Lsjson / CopyTo
src/core/rclone/log.rs      moved verbatim from src/commands/job/rclone_log.rs
src/core/rclone/config.rs   the sandboxed-config generator (below)
```

`src/commands/job/rclone_transfer.rs` is deleted outright; its spawn/
stderr-drain/poll-loop logic becomes `Copy`'s `args()` plus the runner's
`execute_streamed`. `src/commands/job/rclone/` keeps only its two `Job`
implementors, their plan/summary structs, and the wizard.

Implementors declare what's genuinely their own; the runner owns
everything that was duplicated: argv construction, the shared reliability
baseline (`--retries 5 --low-level-retries 20`, `--use-json-log
--log-level INFO --log-file <path>` when the command has one, `--stats
30s` for streamed commands), spawn, the concurrent stderr drain, the poll
loop, and exit-status error text.

```rust
pub(crate) trait RcloneCommand {
    type Output;
    fn subcommand(&self) -> &'static str;
    fn args(&self) -> Vec<String>;
    fn log_path(&self) -> Option<&Path>;
    fn observe(&self) -> Observation;       // Streamed(StreamHooks) | OneShot
    fn labels(&self) -> Labels;
    fn finish(&self, outcome: Outcome) -> Result<Self::Output, String>;
    fn retry(&self) -> Option<(usize, Duration)> { None }
    async fn run(&self) -> Result<Self::Output, String> { /* wraps execute() in retry() if set */ }
}
```

`Copy`/`Purge` are `Streamed` (spawn + live JSON-log tail, ADR-0102);
`Lsjson`/`CopyTo` are `OneShot`. `Lsjson` carries no `log_path` at all --
its own stdout *is* the data it returns, unlike `Copy`/`Purge`/`CopyTo`,
which keep their existing `--use-json-log` operation log. `Lsjson::finish`
keeps the `"directory not found"` → empty-listing tolerance exactly as it
exists today (issue #63's version-fragility, re-homed unchanged). `CopyTo`
is symmetric in direction (`source`/`destination` are both plain strings,
not "local file in, remote string out") so it serves both `transform`'s
push (local → remote, `emit_upload_metrics: true`) and `deduplicate`'s pull
(remote → local, `emit_upload_metrics: false`); `Lsjson`/`CopyTo` both keep
their pre-existing 3-retry/2s-backoff wrapper via `retry()`.

All six prior call sites move onto this trait: `rclone copy`/`delete`
(`rclone/worker.rs`), `transform`'s pull/push (`transform/worker.rs`,
`transform/push.rs`), and `transform`'s two `lsjson` listings
(`transform/destination.rs`). The five duplicate `check_rclone_available`
copies collapse into `core::rclone::check_available()`.

### 2. The sandboxed config generator (`core/rclone/config.rs`)

```rust
/// Sibling to `Store::default_path()`'s `keyring.toml` -- same config
/// directory, one well-known, inspectable artifact.
pub(crate) fn rclone_config_path() -> Result<PathBuf, String>;

/// Writes one `[alias]` section per configured `BucketConfig` to `path`,
/// keyed directly by alias:
///   [alias]
///   type = s3
///   provider = Other
///   access_key_id = <access_key_id>
///   secret_access_key = <secret_key>
///   endpoint = <endpoint>
///   acl = private
///   no_check_bucket = true
/// Never reads an existing file at `path` first -- no merge, ever.
pub(crate) fn write_sandboxed_config(store: &Store, path: &Path) -> Result<(), String>;

/// The rclone remote address for a configured bucket alias -- always the
/// whole bucket's root (`"<alias>:"`). No sub-path affordance.
pub(crate) fn remote_address(alias: &str) -> String;
```

No read of `RCLONE_CONFIG`, no read of rclone's own default config path, no
merge with anything that might already exist at `rclone_config_path()` --
anywhere in this module or anywhere else in the crate. One section per
bucket, keyed by **alias**, not the two-section `alias → bucket-name`
indirection the terraform reference above uses: aliases are already
globally unique (`Store::contains_alias`, ADR-0022), so there is no
collision class to guard against by construction, and no reason to
reproduce an indirection that file's own, unrelated constraints needed.
`provider` is hardcoded to `"Other"` -- `BucketConfig` has, and keeps, no
per-provider field, consistent with ADR-0010's "pigeon's S3 client is
generic" stance. This is a live-unverified assumption against every real
S3-compatible backend pigeon reaches (DigitalOcean Spaces, Backblaze B2,
Scaleway) -- confirmed only against local-filesystem `rclone` operations
in this change's own test suite; flagged in Consequences, not assumed
correct by design alone.

Called once at the start of every rclone-backed job's wizard dispatch,
immediately after loading the keyring `Store` and before constructing any
`RcloneCommand` -- regenerated fresh from the current keyring state every
time, so a `pigeon keyring modify bucket` takes effect on the very next job
run with no cache to invalidate. The shared runner (`core::rclone::execute`)
unconditionally appends `--config <rclone_config_path()>` to every
invocation's argv -- no `Option`, no "only if some bucket-config exists"
branch, since every rclone-backed job needs at least one bucket alias to
address anything in the first place.

### 3. Bucket-alias addressing, not raw paths

`rclone copy`/`delete`'s `--source-path`/`--destination-path` revert to
`--source-bucket`/`--destination-bucket` (ADR-0110's rename reversed --
that rename existed specifically to distinguish a raw passthrough from a
bucket alias; the value **is** a bucket alias again, so the honest name
returns). `transform`'s `--source-path`/`--destination-path` become
`--source-bucket`/`--destination-bucket` the same way. Each flag resolves
to a validated `BucketConfig` alias (erroring on an unknown one, exactly
like `deduplicate`'s existing `--source-bucket`), then to its rclone
address via `remote_address()`. There is no sub-path affordance and no
raw-passthrough mode at all -- unlike the old `source:media/`-style values,
a bucket-config alias addresses one entire bucket, matching
`deduplicate`'s own long-standing model. `--report-bucket` is untouched on
every job, including these four -- ADR-0132 §10's proposed
`--report-path`/`ReportTarget` split is dropped from scope entirely, since
it only existed to support a "some jobs are keyring-free" world this ADR
does not build.

`rclone copy`/`delete`/`transform`'s wizards already loaded the keyring
`Store` before this change (for `--report-bucket` resolution, ADR-0100), so
this is not a new dependency in practice -- it reuses the same `Store`
already in scope, now also used to validate the alias and generate the
sandboxed config.

### 4. `deduplicate`'s transfer mechanism moves onto `core::rclone` (ADR-0133)

`--source-bucket`(repeatable)/`--destination-bucket` and every
`bucket_alias`-named field (`.processed` checkpoint, `ContentIndex`'s 4th
field, report columns, in-memory compound keys) are **unchanged** -- only
the underlying transfer *mechanism* moves off `minio`, not the addressing
model ADR-0133 originally proposed renaming.

- **Listing** (`manifest::gather_pending`): `client::list_objects` →
  `rclone::Lsjson { path: remote_address(&alias), recursive: true, .. }`,
  mapped to `(key: entry.path, size: entry.size)`. `--files-only` already
  excludes directory entries, so the old `entry.is_prefix` check is gone.
- **Pull** (`worker::process_item`): the old `download::download_with_retry`
  per-item call becomes a per-item `rclone::CopyTo { source:
  "<alias>:<key>", destination: <raw_path>, .. }`. Each gets its own small
  JSON log named off the already-unique `raw_path` (`transform::push`'s
  "never share one `--log-file` across concurrent single-file subprocesses"
  discipline). The existing worker-pool architecture (shared queue,
  tainted-root tracking, zip-member requeueing, Apple-double handling) is
  **completely unchanged** -- only this one per-item transfer call inside
  it moves.
- **Upload** (`worker::upload_result`): the old per-file `upload.rs`
  (`pending_upload_tasks`/`run_upload_phase`/`UploadedIndex`) path becomes
  **one bulk** `rclone::Copy { source: <result_dir>, destination:
  remote_address(&alias), .. }`. `--upload-concurrency` is removed
  (meaningless once push is one bulk transfer, not a per-file task queue)
  and `--transfers`/`--checkers`/`--tpslimit` are added, matching every
  other rclone-backed job's tuning flags -- hoisted into
  `shared_wizard.rs` as `TransfersInput`/`CheckersInput`/`TpslimitInput`
  once `deduplicate` became their third consumer (after `rclone::wizard`
  and `transform::wizard`), this codebase's standing "duplicate until the
  third consumer" precedent. `DeduplicateSummary.unchanged` is removed --
  rclone's own size/modtime comparison is this job's "skip if unchanged"
  mechanism now, and there's no equivalent distinct count to report the
  way the old per-file `minio` path produced one.
- **No checkpoint-format change.** Unlike ADR-0133's original proposal
  (`bucket_alias` → `source_path` throughout, a breaking `.staging` format
  change), nothing about the checkpoint's shape changes here -- an
  in-progress `--local-output` directory from before this ADR keeps
  resuming correctly.
- `download.rs`'s `download_with_retry`/`DownloadAnnounce` (and their
  now-unreachable cascade: `format_mb`, `should_announce_download`,
  `DOWNLOAD_RETRIES`, `DOWNLOAD_RETRY_BACKOFF`,
  `ANNOUNCE_DOWNLOAD_THRESHOLD_BYTES`) reach zero consumers once this
  lands and are deleted outright (`clippy -D warnings` forces it, the
  mechanical proof the census was complete). The module is renamed
  `local.rs`, keeping only `check_disk_space`/`sha256_hex`/`sha256_file`/
  `available_disk_space` -- both still used by `transform`, and
  `check_disk_space`/`sha256_file` still used by `deduplicate` itself.

## Consequences

- `rclone copy`/`delete`/`transform` lose the ability to address any
  backend outside pigeon's own keyring. Confirmed acceptable directly --
  real usage has only ever addressed pigeon-managed buckets -- and stated
  here as a removed capability, not silently dropped.
- **Breaking CLI change, no migration shim**, on four job variants,
  consistent with this codebase's established precedent (ADR-0017,
  ADR-0094, ADR-0096, ADR-0103, ADR-0110, ADR-0114). `rclone copy`/`delete`
  and `transform`'s `--source-path`/`--destination-path` become
  `--source-bucket`/`--destination-bucket`; `deduplicate`'s
  `--upload-concurrency` is replaced by `--transfers`/`--checkers`/
  `--tpslimit`.
- `deduplicate` acquires a hard `rclone`-on-`PATH` dependency it did not
  have before, and its data legs are no longer reachable via `minio` at
  all. `minio` remains in this crate only for `email-sync`/`email-pull`'s
  uploads and every job's `--report-bucket` leg.
- `provider = "Other"` is unverified against real S3-compatible backends --
  this change's own test suite only exercises local-filesystem `rclone`
  operations (real subprocess, real argv, skipped rather than failed if
  `rclone` is absent from `PATH`). A live smoke test per backend
  (DigitalOcean Spaces, Backblaze B2, Scaleway) is needed before this is
  fully trusted in production.
- Regenerating the sandboxed config fresh at the start of every job run
  means two pigeon invocations running concurrently both rewrite the same
  file; given identical keyring state this is benign, but a `keyring
  modify bucket` racing a running job is a real, low-probability,
  low-impact edge case, named here rather than ignored.
- No metric label values change for the four implementors that already
  existed in some form (`Copy`/`Purge`'s `pigeon_job`/`phase`/`outcome`
  series are unchanged); `Lsjson` gains observability it never had before
  (a tracing span and `record_phase_count` call), closing the gap
  ADR-0132's Context originally identified.
- 19 `rclone_log.rs` tests move to `core/rclone/log.rs` unchanged; new
  per-implementor argv tests, config-generation tests (empty case, one/many
  bucket-config case, never-reads-an-existing-file case, `0600`
  permissions), and `deduplicate`'s existing test suite all pass
  end-to-end against the new mechanism with no change to its own test
  fixtures' expectations beyond the signature changes this ADR makes.

## Out of scope

- Any `BucketConfig`-to-rclone-translation path for a backend type
  `BucketConfig` cannot model (SFTP, other non-S3 remotes) -- confirmed
  unnecessary; rclone has only ever addressed pigeon-managed S3-compatible
  buckets in this deployment.
- A sub-path addressing mode for a bucket-config alias (e.g.
  `alias:subfolder/`) -- every alias addresses its whole bucket root only,
  matching `deduplicate`'s pre-existing model. A future ADR can add this if
  a real need for it surfaces.
- Live verification of `provider = "Other"` against every real backend --
  named above as a Consequence needing a live check, not performed here.
- Migrating `.staging` state from a pre-ADR-0133-draft deduplicate run --
  moot, since this ADR's design never changed the checkpoint's shape in the
  first place.
- Implementing ADR-0132 §10's `--report-path`/`ReportTarget` split --
  dropped from scope entirely; `--report-bucket` stays as-is on every job.

## Amends

ADR-0101 (reverses its "no `BucketConfig`-to-rclone translation" stance and
its "raw passthrough, never a bucket alias" design for `rclone copy`/
`delete`), ADR-0110 (reverses the `--source-path`/`--destination-path`
rename for `rclone copy`/`delete`/`transform`, back to
`--source-bucket`/`--destination-bucket`), ADR-0132 (implements its
`RcloneCommand` trait/runner consolidation, while dropping its §10
`--report-path` proposal entirely), and ADR-0133 (implements its
`deduplicate`-onto-`core::rclone` migration, while keeping the
`bucket_alias` identity/checkpoint shape ADR-0133 itself proposed changing
to `source_path`). Each gets a short forward-pointing "Amendment" section,
per this repo's established convention for amending a merged ADR without
silently rewriting it.

## Verification

- `mise run ci` clean (fmt-check + lint + test).
- `cargo clippy --all-targets --all-features -- -D warnings` clean
  specifically after `download.rs`'s forced deletions -- the mechanical
  proof the zero-consumer census was complete.
- New unit tests: `write_sandboxed_config` (zero bucket-configs → empty
  file; one bucket-config → exactly the expected single-section shape;
  never reads a pre-existing file at its target path first;
  `#[cfg(unix)]` sets `0600` permissions); `remote_address`'s trailing-colon
  shape.
- Every `core::rclone` implementor's existing real end-to-end test (local
  directories, no `rclone.conf` needed for a bare path) passes against the
  new trait/runner unchanged, the regression check that the consolidation
  is behavior-preserving.
- `deduplicate`'s full existing test suite (worker, dedup, manifest,
  wizard) passes against the new pull/upload mechanism with no change to
  checkpoint-shape expectations.
- `tests/cli.rs`: flag-surface flips confirmed for all four job variants
  (`--source-bucket`/`--destination-bucket` replace `--source-path`/
  `--destination-path` on `rclone copy`/`delete`/`transform`;
  `deduplicate`'s `--upload-concurrency` is gone, `--transfers`/
  `--checkers`/`--tpslimit` present).
- Manual, deferred to a real deployment: an end-to-end `rclone copy`/
  `transform`/`deduplicate` run addressing a configured bucket alias
  against each real S3-compatible backend this deployment uses, confirming
  `provider = "Other"` actually works end-to-end and that the run succeeds
  with **no** `rclone.conf` present anywhere on the host.

## Amendment (2026-10-12): `transform`'s `--source-bucket`/`--destination-bucket` become optional, inferred from `--input-file-type` (ADR-0137)

ADR-0137 renames `transform`'s `--source-bucket`/`--destination-bucket` to
`--source`/`--destination` and makes both optional: when omitted, the
bucket-config alias defaults to `input_file_type.extension()` (source) or
`input_file_type.output_extension()` (destination) -- a documented
bucket-naming convention, not auto-detected content inspection. An
explicitly-passed flag is still validated against the keyring exactly as
this ADR originally specified; inference only applies when the flag is
omitted. `rclone copy`/`delete`/`deduplicate`'s own `--source-bucket`/
`--destination-bucket` are untouched -- this amendment is `transform`-only.
