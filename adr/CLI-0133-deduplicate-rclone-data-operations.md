# CLI-0133: migrate `deduplicate`'s data operations from `minio` to `core::rclone`

- **Author**: Willow Graysen ([@noisypigeon](https://github.com/noisypigeon)).
- **Date**: 2026-10-12.
- **Status**: Accepted -- implemented as part of CLI-0136 (with the §1/§6 modifications that amendment records).

## Context

`pigeon job run deduplicate` (CLI-0082, amended by CLI-0089/0090/0091/0095/
0098/0099/0109/0111/0132) is the last job whose data legs still go through the
`minio` crate. It reaches a bucket at exactly three points:

| Leg | Today | Site |
|---|---|---|
| List source | `client::list_objects(bucket, secret, "", true)` | `deduplicate/manifest.rs:147` |
| Download one object | `download::download_with_retry` -> `client::download_object_to_file` | `deduplicate/worker.rs:198` |
| Upload result | `upload::run_upload_phase` -> `client::upload_if_changed` | `deduplicate/worker.rs:757-776` |

Everything else this job does -- zip expansion, SHA-256 content hashing,
`ContentIndex` lookup/commit, placement, the merge report -- is pure
local-disk work that never touches a bucket. That narrowness is what makes
the migration tractable: three call sites, all already isolated behind
wrappers.

Three things make it worth doing now rather than later:

1. **`deduplicate` is the sole remaining consumer of two `minio` entry
   points.** A full consumer census confirms `client::list_objects` and
   `client::download_object_to_file` have **zero** consumers outside this
   job, and that `download.rs`'s `download_with_retry`/`DownloadAnnounce`
   have zero outside it too. The entire `minio` *download* path in this
   crate exists to serve one job.
2. **CLI-0132 already made `rclone` mandatory for this job.** Its report leg
   (`--report-path`) now shells out to `rclone copyto`, so the binary is a
   hard runtime dependency regardless. Keeping a parallel `minio` data path
   alongside it is the remaining inconsistency, not a hedge.
3. **It unlocks arbitrary rclone remotes.** `deduplicate` can currently only
   reach pigeon's own keyring-managed S3-compatible `BucketConfig` buckets;
   `rclone copy` and `transform` have reached any configured rclone remote
   since CLI-0101/0112.

### This ADR supersedes CLI-0113

**CLI-0113 already decided this migration** (2026-10-10, `Status: Proposed`,
never implemented). Its three-phase pipeline, its `--source-bucket` ->
`--source-path` rename, its decision to keep `--upload-only`, and its
decision to keep the pre-run preview table all stand and are carried forward
here. But it has since been invalidated in four concrete ways, so this ADR
restates the design authoritatively rather than leaving a stale document as
the live reference:

1. **Its mechanism no longer exists.** CLI-0113 §2 builds on
   `rclone_transfer::run_rclone_copy`, which CLI-0132 deletes. The mechanism
   is now `core::rclone`'s `RcloneCommand` trait and its `Copy`/`Lsjson`
   implementors.
2. **One of its Consequences is factually wrong.** CLI-0113 states that
   `download_with_retry`/`DownloadAnnounce`/`check_disk_space` "drop to one
   remaining caller (`pull_transform`)" and that "neither module becomes dead
   code -- no removal follows from this ADR." CLI-0131 deleted
   `pull_transform` entirely. Two of those three drop to **zero** consumers,
   and `clippy -D warnings` makes their removal mandatory, not optional.
3. **Its report-flag row is stale.** CLI-0113's §1 table records
   `--report-bucket` as "unchanged -- orthogonal to this refactor";
   CLI-0132 has since renamed it to `--report-path` for this job.
4. **Its pipeline would make a run slower than today.** CLI-0113 §2
   describes a strict Phase A -> Phase B barrier, which discards the
   download/process overlap the current implementation already has (see §4).
   It also omits the `source_bucket` metric-label question entirely, which
   has no obvious answer once aliases disappear (see §7).

CLI-0113's `Status` is updated to record that it was superseded before any
code landed, following the precedent CLI-0035 set for a design that never
reached `main`.

## Decision

### 1. Command surface

| Flag | Before | After |
|---|---|---|
| Source | `--source-bucket` (repeatable, keyring alias, CLI-0109) | `--source-path` (repeatable, raw rclone `remote:path`) |
| Destination | `--destination-bucket` (optional, keyring alias) | `--destination-path` (optional, raw rclone `remote:path`) |
| Transfer tuning | none (per-object concurrency only) | `--transfers`/`--checkers`/`--tpslimit` (defaults 8/16/unset, matching `rclone copy`) |
| Upload concurrency | `--upload-concurrency` (CLI-0091) | **removed** -- meaningless once push is one bulk `rclone copy` rather than a per-file task queue |
| `--concurrency` | sizes the download+hash pool | same flag, now bounding purely local CPU work |
| `--upload-only` | CLI-0089 | kept (see §5) |
| `--local-output`, `--report-path`, `--non-interactive` | — | unchanged (CLI-0132, CLI-0114) |

`--source-path`/`--destination-path` are opaque strings passed straight into
`rclone`'s argv, never resolved against the keyring -- the same convention
`rclone copy` and `transform` already use, and the same explicit rejection of
a `BucketConfig`-to-rclone translation layer that CLI-0113 made.

`SourceBucketsInput` (`deduplicate/wizard.rs:32-91`) and its
`resolve_bucket` helper (`:98-106`) are replaced by a repeatable raw-path
input modeled on `rclone/wizard.rs`'s `SourcePathInput`; the shared
`UploadTargetInput` use (`wizard.rs:295-317`) becomes a
`--destination-path` input. Combined with CLI-0132 having already removed
the report leg's keyring need, **`deduplicate` becomes entirely
keyring-free** -- no `keyring.toml`, no OS-keychain access, no
`PIGEON_SECRET_*` -- joining `transform` and `rclone copy`/`delete`.

### 2. Three legs, each one `core::rclone` command

```
Phase A (pull)     rclone::Lsjson { recursive: true }   per --source-path   (preview + files-from)
                   rclone::Copy   <source-path> -> .staging/source/<n>/     (files_from, on_copied)
Phase B (local)    unchanged: zip-expand -> hash -> ContentIndex -> place -> checkpoint
Phase C (push)     rclone::Copy   <local-output>/result -> <destination-path>
```

- **Preview** reuses `rclone::Lsjson` (CLI-0132 §4) rather than
  `list_objects`. It already returns `Vec<LsjsonEntry>` and already tolerates
  a non-existent path as an empty listing, and it now carries the
  tracing/metric instrumentation CLI-0132 added. Its output feeds the same
  extension-keyed `TypeSummary` table the wizard prints pre-confirm today
  (`manifest.rs:58-62`, `wizard.rs::print_type_summary`) -- still one
  combined cross-source table, not broken out per source, preserving
  CLI-0109's deliberate "treat as one combined set" framing.
- **Pull** uses `rclone::Copy` with `job: "deduplicate"`, `step: "pull"`,
  `upload_metrics: false`. Each `--source-path` lands in its own numbered
  `.staging/source/<n>/` subdirectory, so provenance survives into Phase B's
  report and checkpoint without needing any bucket-alias concept.
- **Push** uses `rclone::Copy` with `step: "push"`,
  `upload_metrics: true`, replacing `upload_result`'s
  `pending_upload_tasks` + `run_upload_phase` pair wholesale.

`deduplicate` becomes `core::rclone`'s fifth consumer, after `rclone copy`,
`rclone delete`, `transform`'s pull, and `transform`'s push.

### 3. Already-processed objects are never transferred, reusing CLI-0124

Today `gather_pending` (`manifest.rs:136-177`) consults the `.processed`
checkpoint and skips already-finished keys *before* any download happens. A
naive bulk `rclone copy` would discard that and re-pull the entire source
set on every resumed run.

CLI-0124 already solved exactly this problem for `transform`: list the source
with `lsjson`, subtract everything already done, and pass the remainder to
`rclone` as `--files-from-raw`. `deduplicate` reuses that mechanism directly
via `rclone::Copy`'s `files_from` field (CLI-0132 §7), so an
already-processed object is never transferred at all.

This is a correctness requirement, not an optimization. Without it, resuming
an interrupted run against a large source re-downloads everything it already
processed -- a regression against today's behavior, not merely a missed
improvement.

### 4. Pull and local work must overlap -- correcting CLI-0113

CLI-0113 §2 describes a strict Phase A -> Phase B barrier: pull everything,
then walk the local tree. That would make a `deduplicate` run **slower than
it is today**, because the current implementation already overlaps the two:
`run_deduplicate_job` (`worker.rs:397-641`) runs a hand-rolled worker pool
(a shared `Arc<Mutex<VecDeque<QueueItem>>>` plus an in-flight counter, not a
`Semaphore`) in which each worker downloads an object and then immediately
expands and hashes it, so network and CPU work proceed concurrently.

Phase B therefore consumes `rclone::Copy`'s `on_copied` channel (CLI-0116,
carried in CLI-0132's `StreamHooks`), which reports each object's relative
path the instant rclone's own JSON log confirms its bytes are fully on disk.
This is precisely what `transform/worker.rs:734-750` already does with its
own pull. The existing worker pool is retained unchanged; only its work
*source* changes -- from "download this key yourself" to "this local file
just landed."

Stated plainly: this is a correction to CLI-0113's design, which would have
regressed throughput, not a silent enhancement bolted onto it.

### 5. `--upload-only` is kept

Preflight keeps its current shape (`upload_only_preflight_ok`,
`wizard.rs:413-423`: `.staging/.processed` must exist and `result/` must be
non-empty), and `--destination-path` stays mandatory in this mode exactly as
`--destination-bucket` was. The mode skips Phase A entirely and runs Phase C
only, preserving CLI-0089's original guarantee that resuming a crashed upload
needs no access to the source remote at all -- and now, no `rclone.conf`
entry for it either.

This is a deliberate departure from `transform`, which dropped its equivalent
flag in favor of full-rerun resumability (CLI-0112 §6). That trade only makes
sense when every phase is cheaply re-enterable, and `deduplicate`'s pull is a
full recursive copy potentially spanning several source paths at real scale --
not cheap enough to redo on every resumed run.

`--upload-concurrency` disappears, so CLI-0091's note that it is "the only
concurrency flag that has any effect" under `--upload-only` becomes moot;
`--transfers`/`--checkers` take that role.

### 6. Identity migration: `bucket_alias` -> `source_path`

CLI-0109 threaded bucket identity through considerably more than the transfer
call itself. Each of the following changes meaning, and all of them are in
scope -- enumerated here rather than left for implementation to rediscover,
since a missed one is exactly the silent-skip class of bug CLI-0030/0034/
0098/0099 each previously root-caused in this job:

- **In-memory**: `DeduplicateTask.bucket_alias` (`manifest.rs:51`),
  `QueueItem.bucket_alias` (`worker.rs:62`, copied onto every zip-member
  descendant at `:270`), `HashedFile.bucket_alias` (`dedup.rs:39`), and the
  compound-keyed `finished_root_keys` (`worker.rs:418`), `tainted_roots`
  (`:428`), `apple_double_skipped_roots` (`:433`), and `place_and_report`'s
  `finished_keys` (`dedup.rs:101`).
- **`.staging/.processed`**: lines change from `{bucket_alias}\t{key}` to
  `{source_path}\t{relative_path}` (`manifest.rs:110-126` write, `:80-94`
  read, `:101-108` lookup). Same `split_once('\t')` discipline. CLI-0109's
  legacy no-tab sentinel is **dropped**: a pre-migration checkpoint is
  structurally incompatible either way (see Consequences), so keeping a
  fallback that silently half-works would be worse than requiring a fresh
  directory.
- **`.staging/.content-hashes`**: `ContentIndex`'s 4th tab-separated field
  (`core/data.rs:45-53`) changes meaning from `source_bucket_alias` to
  `source_path`. `check_with_original_key_and_bucket` and
  `commit_with_key_and_bucket` (`data.rs:131`/`:193`) -- both of which
  `deduplicate` is the only caller of -- are renamed to `_and_source`. The
  lenient 2-/3-/4-field `load` is untouched, and `email_sync`/`email_pull`'s
  own `ContentIndex` use is unaffected.
- **`deduplicate-report.txt`**: CLI-0109's two trailing columns
  `duplicate_bucket_alias`/`kept_bucket_alias` (`dedup.rs:211-213`) become
  `duplicate_source_path`/`kept_source_path`. Still appended last, so
  anything parsing the first four columns positionally keeps working --
  the same append-don't-reorder discipline CLI-0099 and CLI-0109 each used.
- **`.staging/.uploaded`**: no longer used by `deduplicate` at all,
  superseded by `rclone copy`'s own incremental size/modtime skip -- the
  mechanism `rclone copy` and `transform` already rely on for idempotent
  reruns. `upload.rs`'s `UploadedIndex` survives untouched for the two email
  jobs.

### 7. Metrics

- `pigeon_job="deduplicate"` is unchanged (no job rename). New
  `phase="pull"|"push"` labels on `pigeon_job_phase_total`, emitted via
  `rclone::Copy`'s delta polling, replace the per-object download/upload
  phase counts the `minio` path produced.
- **`source_bucket` carries the raw `--source-path` string.** This is a
  deliberate, documented exception to the rule CLI-0102 established for
  `import` and CLI-0130 reaffirmed for `transform`, both of which route a raw
  `remote:path` to the `NO_BUCKET` sentinel specifically to keep label
  cardinality bounded. The exception is justified here because this
  deployment provisions `rclone.conf` from the same automated process that
  provisions the keychain, so the set of remote names a run can reference is
  stable and small -- the unbounded-cardinality risk those ADRs guarded
  against does not apply. The alternative (collapsing to `NO_BUCKET`) would
  discard CLI-0109's per-source attribution, which exists precisely so a
  multi-source run can be read per source. Recorded as an exception with its
  reasoning so a future reader does not mistake it for an oversight.
- `pigeon_upload_bytes_total`/`pigeon_upload_outcomes_total` are emitted for
  Phase C only; a pull is a download, not an upload. Per CLI-0102's
  already-filed gap ([#28](https://github.com/noisypigeon/pigeon-cli/issues/28)),
  `pigeon_upload_attempts_total`/`pigeon_upload_duration_seconds` -- genuinely
  per-file measurements rclone's aggregate JSON stats cannot produce without
  `-v`-level logging this job's flag set does not enable -- become
  unavailable for `deduplicate` too, the same as every other rclone-backed
  job. Stated rather than silently regressed.
- `set_macro_phase("deduplicate", false)` stays at Phase B start
  (`worker.rs:382`); the `true` transition currently fires inside
  `upload::run_upload_phase` (`upload.rs:373`), so once `deduplicate` stops
  calling that, it must set it itself at Phase C start.
- **Disk preflight**: the per-item `check_disk_space(raw_dir, item.size)`
  before each download (`worker.rs:177`) has no bulk-pull equivalent, so a
  pre-pull check is added instead, following the pattern CLI-0130 added to
  `transform`. The pre-zip-expansion check (`worker.rs:238`) stays per-item,
  unchanged.

### 8. Forced deletions -- the consumer census, stated explicitly

`clippy --all-targets --all-features -- -D warnings` is a hard gate here, so
anything reaching zero consumers must be deleted rather than left behind.
These are `pub(crate)` (and therefore `dead_code`-exposed) at **zero**
consumers once `deduplicate` stops using them:

- `download.rs`: `DownloadAnnounce` (and its `new`/`start`/`finish`),
  `download_with_retry`, plus the cascade only they reach -- `format_mb`,
  `should_announce_download`, `DOWNLOAD_RETRIES`,
  `DOWNLOAD_RETRY_BACKOFF`, `ANNOUNCE_DOWNLOAD_THRESHOLD_BYTES`.
- `deduplicate/worker.rs`: `upload_result` (`:752`), `run_upload_only`
  (`:787`), `log_upload_phase_complete` (`:737`).

Zero-consumer but **not** a clippy failure, because they are `pub` on a
fully-`pub` module path and so exempt from `dead_code` -- deleted for
hygiene, with that distinction stated so the deletion does not look
load-bearing: `client::list_objects`, `client::download_object_to_file`, and
`client::list_buckets` (the last already dead since CLI-0010, as CLI-0017
recorded).

**Explicitly not dead**, despite appearances -- recorded so a later reader
does not over-prune:

- All of `upload.rs`. `email_sync` and `email_pull` keep every `pub(crate)`
  item alive (`run_upload_phase` has 4 remaining call sites,
  `UploadedIndex` 8).
- Every `pub(crate)` item in `client.rs`: `upload_if_changed` still serves
  `upload.rs` and `report_upload.rs`, and `bucket_exists` still serves
  `keyring/wizard.rs`.
- All of `core::crypto` and the encryption-key keyring plumbing.
  `deduplicate` already passes `None` as its encryptor (`worker.rs:772`), so
  it contributes nothing to that path; `email_sync` (encrypt) and
  `decrypt_files` (decrypt) are unaffected.
- All of `shared_wizard.rs`; `check_disk_space` and `sha256_hex` (both kept
  alive by `transform`); and `ContentIndex::load`/`check`/`commit`.
- `sha256_file`. `deduplicate` keeps hashing every pulled file locally --
  only its *transfer* mechanism changes, not its dedup logic.

### 9. `download.rs` is renamed to match what it holds

With `download_with_retry` and `DownloadAnnounce` deleted, the module
contains `available_disk_space`/`check_disk_space`/`MIN_FREE_DISK_BYTES` and
`sha256_hex`/`sha256_file` -- no download code whatsoever, making both its
name and its header doc comment actively misleading.

It is renamed to `src/commands/job/local.rs`, with a header doc describing it
as the shared disk-space and content-hashing helpers. Call sites to update:
`deduplicate/worker.rs:177`/`:238`/`:302`, `transform/worker.rs:345`/`:699`,
`transform/placement.rs:50`, and the module declaration at
`src/commands/job/mod.rs`. CLI-0131 kept this module shared on the grounds
that `transform` genuinely needs it; that reasoning survives, and this is
only a rename to stop the name lying about the contents.

### 10. Two open issues this ADR resolves or invalidates

Both are checked against the live `out-of-scope` issue list and addressed
explicitly rather than left dangling:

- [#19](https://github.com/noisypigeon/pigeon-cli/issues/19) ("download:
  small-file announce threshold buries large-file call-outs") concerns
  `DownloadAnnounce` and `ANNOUNCE_DOWNLOAD_THRESHOLD_BYTES`, both of which
  §8 deletes. It becomes moot and is closed with a pointer to this ADR.
- [#17](https://github.com/noisypigeon/pigeon-cli/issues/17) ("skip
  downloading/hashing duplicates already proven identical by size+ETag")
  assumes an S3 `ETag` available from `list_objects` before download. After
  this migration there is no `list_objects` and no ETag, so its stated
  mechanism is gone -- but `rclone lsjson` can emit per-object hashes
  (`--hash`), so the underlying idea survives in changed form. The issue
  stays **open**, annotated that its mechanism needs rethinking against
  `lsjson`. This migration does not deliver it, and this ADR does not claim
  it does.

## Consequences

- **Breaking CLI change, no migration shim** -- consistent with this
  codebase's standing precedent (CLI-0017, CLI-0094, CLI-0096, CLI-0103,
  CLI-0110, CLI-0132). `--source-bucket`, `--destination-bucket`, and
  `--upload-concurrency` disappear; every cron entry, deployment script, and
  runbook invoking them must move to `--source-path`/`--destination-path`
  before this ships.
- **Pre-existing `.staging` state is structurally incompatible and cannot be
  reused.** A pre-migration `.processed` is keyed `(bucket_alias, key)` and
  `.content-hashes`' 4th field holds a bucket alias; both now mean source
  paths. A resumed run needs a **fresh `--local-output` directory**, not a
  reused one. No conversion or detection is provided. This is the sharpest
  operational edge of this change: silently reusing an old directory would
  mis-attribute provenance in the report and could mis-skip work.
- `deduplicate` becomes entirely keyring-free. Combined with CLI-0132, the
  keyring now serves only `email-sync`, `email-pull`, and `decrypt-files`.
- The `minio`-based **download** path leaves this crate completely. `minio`
  remains only for `email-sync`/`email-pull`'s uploads and for every job's
  report leg.
- Any Grafana/Cockpit panel keyed on `deduplicate`'s old per-object phase
  labels must move to `phase="pull"|"push"`, and `source_bucket` values
  change from keyring aliases to raw rclone paths. Unlike CLI-0132 (which
  changed no label values), this job's series do break.
- `deduplicate` can now dedup from and to any rclone remote, not only
  pigeon's keyring-managed S3-compatible buckets -- a real capability
  expansion, but pigeon's own credential management no longer covers any of
  this job's legs.
- Resuming an interrupted run no longer re-transfers already-processed
  objects (§3), and `--upload-only` no longer needs an `rclone.conf` entry
  for the source at all (§5).

## Out of scope

- Any `BucketConfig`-to-rclone-remote translation layer -- rejected by
  CLI-0113 and reaffirmed by CLI-0132; `--source-path`/`--destination-path`
  are raw passthrough strings only.
- Migrating or converting `.staging` checkpoint files written by a
  pre-migration run.
- Touching `email-sync`/`email-pull`'s `minio` upload path, or any other job
  type's bucket interactions -- this ADR is scoped to `deduplicate` alone.
- Adding client-side encryption to `deduplicate` -- it has never offered it
  (CLI-0082 §0) and this does not change that.
- Dropping `--upload-only` or the pre-run preview table -- both explicitly
  kept (§5, §2).
- Source-path provenance on `archive::ArchiveFailure` -- still the gap
  CLI-0109 deferred: the report's manual-attention section cannot say which
  source a failed zip came from, unlike its merge rows. Filed as
  [#66](https://github.com/noisypigeon/pigeon-cli/issues/66) alongside this ADR
  per CLI-0031.
- Per-source `--transfers`/`--checkers` tuning -- one flat set of transfer
  flags applies to every `--source-path`, matching how `--concurrency`
  already works across sources.
- Implementing [#17](https://github.com/noisypigeon/pigeon-cli/issues/17)'s
  hash-based pre-download skip against `rclone lsjson --hash` (§10).
- The implementation itself -- this ADR documents the design; a follow-up PR
  implements it, matching CLI-0113's and CLI-0132's precedent.

## Amends

CLI-0082 (the original architecture -- its three bucket legs replaced),
CLI-0089 (the streaming `minio` upload mechanism, no longer used by this job;
its `--upload-only` guarantee explicitly preserved), CLI-0091
(`--upload-concurrency`, removed for this job), CLI-0099 (the
`kept_original_key` column stays; `ContentIndex`'s 4th field changes
meaning), CLI-0109 (bucket-alias identity generalized to source paths
throughout -- checkpoint, content index, report columns, and every in-memory
compound key), CLI-0113 (**superseded**; its `Status` updated to record that
no code from it landed), and CLI-0132 (`deduplicate` becomes `core::rclone`'s
fifth consumer; `download.rs` renamed to `local.rs`). Draws on without
amending: CLI-0101/CLI-0110 (flag naming and subprocess conventions),
CLI-0112 (the pull -> local work -> push template), CLI-0116 (`on_copied`),
CLI-0124 (`--files-from-raw` skip-before-pull), and CLI-0130 (disk-space
preflight). Each amended ADR gets a short forward-pointing "Amendment"
section, per the pattern CLI-0101 already carries.

## Verification

- `mise run ci` clean (fmt-check + lint + test), and
  `cargo clippy --all-targets --all-features -- -D warnings` clean
  specifically *after* the §8 deletions, which is the mechanical check that
  the census was complete.
- New unit tests: the `(source_path, relative_path)` checkpoint round-trips
  correctly including the multi-source case; `deduplicate-report.txt`'s
  renamed columns reflect the correct source for a file pulled from the
  second of two `--source-path` values; the `Lsjson`-driven preview builds
  the expected `TypeSummary` from a sample listing.
- `core::rclone`'s own tests (written for CLI-0132's `rclone copy`/
  `transform` consumers) pass unchanged with `deduplicate` as a fifth
  consumer -- the regression check that the trait really is generic.
- `tests/cli.rs`: `deduplicate --help` shows `--source-path` (repeatable),
  `--destination-path`, `--transfers`, `--checkers`, `--tpslimit`, and
  `--upload-only`, and no longer shows `--source-bucket`,
  `--destination-bucket`, or `--upload-concurrency`. Missing `rclone` on
  `PATH` fails fast before any prompt.
- Manual, end-to-end against plain local directories (rclone treats a bare
  path as local, needing no `rclone.conf` -- the same approach CLI-0112/0113
  used): a mix of plain files plus a zip containing a duplicate, confirming
  placement, report columns, and checkpoint are all correct; then a **rerun
  of the identical command**, confirming already-processed objects are not
  re-transferred at all (§3's guarantee, checked against the pull phase's own
  transferred count, not just the final result); then a third run with
  `--upload-only`, confirming the push-only path succeeds without touching
  the source.
- Manual, against two real `rclone.conf`-backed remotes passed as two
  `--source-path` values, confirming `pigeon.jsonl` carries
  `phase="pull"`/`phase="push"` lines and that `source_bucket` label values
  are the raw source paths (§7).

## Amendment (2026-10-12): implemented as part of CLI-0136 -- `--source-path`/`--destination-path` rename and `source_path` identity migration both dropped

This ADR's mechanism (§2-§5: `Lsjson` for listing, `Copy` for pull with
`on_copied`/`files_from`, bulk `Copy` for push, `--upload-only` kept,
`--transfers`/`--checkers`/`--tpslimit` added, `--upload-concurrency`
removed) is implemented exactly as described here, as part of CLI-0136.
Two things this ADR proposed do **not** happen:

- **§1's flag rename never lands.** `--source-bucket`/`--destination-bucket`
  stay exactly as they are today -- still repeatable, still keyring
  aliases, still validated against the keyring. CLI-0136 restores bucket
  aliases as the addressing model for every rclone-backed job, so the
  "raw passthrough, never a bucket alias" distinction this ADR's rename
  existed to draw no longer applies.
- **§6's `bucket_alias` → `source_path` identity migration never lands.**
  Every `bucket_alias`-named field (`.staging/.processed`'s second column,
  `ContentIndex`'s 4th field, `deduplicate-report.txt`'s trailing columns,
  every in-memory compound key) keeps its existing name and shape. A
  pre-CLI-0136 `--local-output` directory's checkpoint state keeps
  resuming correctly -- there is no breaking state-format change here,
  unlike this ADR's own §6 and Consequences, which assumed one.
- §7's `source_bucket` metric-label exception is therefore moot: the label
  already carries a keyring alias, the same bounded-cardinality value every
  other job's `source_bucket` label already carries, so no exception to
  CLI-0102/CLI-0130's raw-path-label rule is needed.

## Amendment (2026-10-15): `--report-path` never landed either (CLI-0136)

The amendment above retracts §1's data-leg rename but says nothing about the
report flag, and this ADR's Context (point 2), point 3, and §1's flag table
all describe `--report-path` as already done by CLI-0132. It never was:
CLI-0136 dropped CLI-0132 §10 before it landed, so `deduplicate` still takes
`--report-bucket`, a keyring bucket-config alias, like every other job. With
`--source-bucket`/`--destination-bucket` also unchanged, `deduplicate`
remains keyring-dependent. The rclone-on-`PATH` dependency point 2 argues
from still holds, because the report upload does go through `rclone copyto`
(CLI-0148), just addressed by alias.
