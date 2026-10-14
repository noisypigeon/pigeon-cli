# ADR-0148: drop `minio`, and fix the secret override it was hiding

- **Author**: Willow Graysen ([@noisypigeon](https://github.com/noisypigeon)).
- **Date**: 2026-10-13.
- **Status**: Accepted.

## Context

ADR-0147 removed `email-sync`, `email-pull`, `job/upload.rs`, and client-side encryption. That left the `minio` crate with exactly **two** call sites, neither of them on a job's data path:

| Site | What it does | Introduced |
|---|---|---|
| `report_upload::upload_one` (`report_upload.rs:163`) | uploads a run's `report.txt`, `transcript.txt`, and `pigeon.jsonl` | ADR-0100 |
| `keyring::wizard::check_bucket_exists` (`keyring/wizard.rs:42`) | verify-before-save on `keyring add`/`modify bucket` | ADR-0010 |

Both are confined to `src/commands/keyring/bucket/client.rs`. Moving them onto ADR-0132's `core::rclone` makes every S3 access in the crate go through one abstraction and lets that file, `minio`, and `md5` go. ADR-0147 §3 recorded `minio`/`md5`/`client.rs` as "explicitly not dead, despite appearances"; this ADR is what resolves that standing exception.

### A latent bug the migration would otherwise regress

`write_sandboxed_config` (`core/rclone/config.rs:106-112`) writes `bucket_config.secret_key` **straight out of the keyring**, never through `credentials::resolve_secret`. So ADR-0087's `PIGEON_SECRET_<ALIAS>` override:

- **works** today for the two `minio` sites above, because both resolve the secret themselves before calling, and
- **has never worked** for any rclone-addressed operation -- `transform`'s pull and push, `deduplicate`'s three legs (ADR-0133), `rclone copy`/`delete`. Which is every job's actual data path.

It has gone unnoticed because the deployment keeps bucket secrets in `keyring.toml` directly and uses the override for IMAP identities, where it does work. But it means a naive migration of report upload to rclone would **silently stop honouring `PIGEON_SECRET_<REPORT_BUCKET>`** -- a regression wearing a refactor's clothes. Fixing the generator instead makes the override behave as ADR-0087 always claimed, everywhere. §1 therefore comes first: it is a prerequisite, not a consequence.

### Dead weight the deletion finally reaches

`client.rs` is `pub` on a fully-`pub` module path (`lib.rs` -> `commands` -> `keyring` -> `bucket`), so `dead_code` cannot see into it and `clippy -D warnings` has never objected to four public functions with zero callers: `ObjectEntry`, `list_buckets`, `list_objects`, `download_object_to_file`. `list_buckets`'s own doc comment has admitted since ADR-0017 that it "no longer backs a standalone CLI command ... kept as a plain function for reuse"; reuse never arrived across ~130 ADRs. `UploadOutcome` is vestigial in a subtler way -- both variants are *constructed*, so the lint stays quiet, but the sole caller discards the value through `if let Err(..)`, so nothing in the crate ever reads it.

That matters beyond tidiness: the entire ETag recipe (`UPLOAD_PART_SIZE`, `HASH_CHUNK_BYTES`, `hash_file_part`, `expected_etag_for_file`, `expected_etag_with_part_size`, `needs_hash`, `file_size`) and the whole `md5` dependency exist to compute a value that only `UploadOutcome::Unchanged` would have communicated. Three small artifacts per run are paying for a multipart-aware content-hash comparison nobody reads.

**Intended outcome:** no `minio`, no S3 code path that is not `rclone`, and `PIGEON_SECRET_<ALIAS>` honoured uniformly.

## Decision

### 1. `write_sandboxed_config` honours `PIGEON_SECRET_<ALIAS>`

`config.rs` resolves each bucket's secret through `credentials::resolve_secret(&bucket_config.alias, &bucket_config.secret_key)` instead of reading the field directly.

Stated plainly: this is a bug fix that happens to be load-bearing for §2, not a side effect of it. Without it, migrating report upload off `minio` would take a working override away.

It is also a **behaviour change** for any environment that sets `PIGEON_SECRET_<BUCKET_ALIAS>` today: `transform`, `deduplicate`, and `rclone copy`/`delete` go from silently ignoring it to honouring it. That is ADR-0087's documented intent, so it is a correction rather than a new feature, but a deployment that set the variable to a *stale* value while relying on `keyring.toml` would newly pick the stale one up. Recorded in `## Consequences` rather than buried here.

### 2. `report_upload::upload_one` -> `CopyTo`

The three artifact uploads are already best-effort: `tracing::warn!` on failure, never touching the job's exit code (ADR-0100). `CopyTo` is the same primitive `transform` and `email_import` already push single objects with. The destination composes as `RemoteTarget::bucket_root(&alias).address()` plus the `{prefix}/{name}` key the function already builds.

rclone reads credentials from the generated config, so **the secret leaves the whole report-upload path**:

- `report_upload::resolve` returns `BucketConfig` rather than `(BucketConfig, String)`.
- `upload_run_artifacts` drops its `secret` parameter.
- The `report_secret` local disappears from six call-site pairs across five wizards (`rclone` x2, `deduplicate` x2, `email_import`, `transform`).

`deduplicate/wizard.rs` additionally resolves secrets for its source and destination buckets even though ADR-0133 moved its data legs to rclone. Those are pruned on `clippy -D warnings`' say-so rather than by inspection -- the same discipline ADR-0147 §3 used, and the one this ADR's own Verification leans on after that change shipped two unused dependencies past a clean gate.

Losing `upload_if_changed` also loses its skip-if-unchanged behaviour: a rerun now re-uploads an identical artifact rather than comparing ETags first. For three small files per run, written to a fresh `{date}-{job}-{run-id}/` prefix every time (ADR-0100), that comparison could never fire anyway -- the key is new on every run by construction. Noted so the loss does not read as overlooked.

### 3. `check_bucket_exists` -> `Lsjson`, against a probe config

ADR-0010's verify-before-save is **kept**. The obstacle is a genuine ordering problem: it validates a bucket *before* that bucket is in the keyring, while `execute` unconditionally appends `--config rclone_config_path()?` (`core/rclone/mod.rs:184-185`) naming a config generated *from* the keyring.

- `RcloneCommand` gains a defaulted `fn config_path(&self) -> Option<&Path> { None }`; `execute` prefers it when `Some`. Defaulted, exactly like `retry()`, so none of the four existing implementors change. `Lsjson` gains the field.
- The probe config is built by pushing the candidate `BucketConfig` into an ephemeral one-entry `Store` and handing that to the **existing** `write_sandboxed_config`. No second config emitter, so ADR-0140's two-section `alias` -> bucket shape cannot drift out of sync -- which is the whole reason that function takes a `&Store` rather than a `&BucketConfig`.
- It is written beside the keyring, in the directory that already holds `keyring.toml` and `rclone.conf` (both `0600`), as `rclone-probe-<pid>.conf`, and removed on every path. Not the OS temp directory: this file carries a secret access key, and the keyring's own directory is the one location already trusted with that (ADR-0135). The pid keeps two concurrent `keyring add`s from colliding.
- The probe itself is `Lsjson { path: "<alias>:", missing_ok: false, job: "keyring", .. }`. `missing_ok: false` is the entire point (ADR-0140): a bucket rclone cannot resolve must fail rather than read as an empty listing.

**Three things stated rather than glossed:**

- **The error gets less precise.** S3's `HeadBucket` answers exists / definitively-absent / could-not-tell, which is what let `add_bucket` say `bucket 'x' does not exist` for a typo. `rclone lsjson` either succeeds or errors. Recovering the distinction would mean matching stderr substrings -- issue [#63](https://github.com/noisypigeon/pigeon-cli/issues/63)'s version-fragile pattern, which ADR-0132 declined to fix and ADR-0140 used only under a narrowly-scoped `missing_ok`. **Decision: do not pattern-match.** The outcome collapses to verified, or `failed to verify bucket '<name>': <rclone stderr>` -- and rclone's own message does say `NoSuchBucket` when that is the cause. ADR-0010's guarantee survives intact; only its phrasing degrades.
- **`no_check_bucket = true`** appears in every generated backend section. It exists to stop an *upload* HEADing or creating a bucket, and should not affect a listing: `ListObjectsV2` against a missing bucket returns `NoSuchBucket` whether or not that flag is set. Reusing `write_sandboxed_config` unmodified therefore ought to work -- but this is the one claim in this ADR that cannot be settled without a live endpoint, so Verification checks it empirically against the real remote rather than asserting it here. If it does mask the error, the fix is a parameter on `write_sandboxed_config`, not a second emitter.
- **`keyring add bucket` and `modify bucket` now require `rclone` on `PATH`.** That is new for a `keyring` command -- previously only jobs needed it (ADR-0145 §5). The preflight reuses `core::rclone::check_available` so the failure names the missing binary instead of surfacing as a confusing verification error.

`job: "keyring"` is a new `pigeon_job` metric label value. It is a fixed string, so it adds no cardinality risk (ADR-0102/0130's rule about never using raw paths as labels), but it is the first value that is not a job name, and `Lsjson`'s `OneShot` path emits one `pigeon_job_phase_total{phase="list"}` per probe.

### 4. Delete and prune

`src/commands/keyring/bucket/client.rs` goes entirely -- ~370 lines, taking with it the four dead `pub` functions, `ObjectEntry`, `UploadOutcome`, the ETag machinery, and five of its six unit tests.

Dependencies pruned: **`minio`** (8 import lines, that file only) and **`md5`** (6 uses, that file only -- its sole purpose was the ETag recipe). `futures` stays: removing `client.rs` costs it one consumer and leaves three.

Two near-misses worth naming, because both would be plausible over-prunes:

- **`async-native-tls` stays** -- IMAP needs it (`keyring/email/imap_client.rs`). `Dockerfile:9-10` justifies installing `pkg-config`/`libssl-dev` via "async-native-tls/minio pull in native-tls -> openssl-sys"; only the `minio` half of that sentence is now wrong. The `apt-get` line and the runtime `libssl3` **stay**. This is the same trap ADR-0131 §4 flagged for `ffmpeg`, inverted.
- **`sha2` and `hex` stay** -- `job/local.rs`'s hashing helpers, not `client.rs`'s.

One unrelated-but-adjacent prune, found while auditing the manifest: **`time`'s `"formatting"` feature is dead.** `report_upload::run_prefix` is the crate's only `time` consumer, and it reads `.year()`/`.month()`/`.day()` and builds the string with `format!` -- there is no `.format(...)` or `format_description!` anywhere in `src/`. Taken here because it is a one-word change in the same manifest edit; called out rather than slipped in.

### 5. Documentation

- **`Dockerfile:9-10`** -- the comment above, `minio` removed from it.
- **`Cargo.toml`'s `description` and `src/cli.rs`'s `#[command(about)]`** both read "Pigeon: authenticate, sink, and transform personal data from external services." `sink` and `transform` as *product verbs* date to ADR-0001/0005/0006, all since reversed; `README.md` was already corrected by ADR-0147. `src/cli.rs`'s copy is what `pigeon --help` prints, so this is user-visible, not just metadata. (The `email_import/sink.rs` *module* is live and correctly named -- not a hit.)
- **`CLAUDE.md`'s Commands section** illustrates the `mise run pigeon` task with `-- email list`. The `pigeon email` group was removed by ADR-0022; that command errors today. A live instruction, not history.
- **`.claude/skills/analyze-job-run/SKILL.md`** still names `pigeon_upload_attempts_total` in step 4's crash-detection bullet, while the same file now states a few sections later that the metric "no longer exist[s] (ADR-0147)". ADR-0147 fixed the second mention and missed the first, so the runbook currently contradicts itself. Fixed here, and worth recording as the second time that file has drifted in the same way.
- **`.github/scripts/adr-issue.sh`**'s label descriptions: `area:keyring` still says "Identities, bucket configs, and encryption keys in the OS keychain" (two errors -- ADR-0135 retired the keychain, ADR-0147 removed encryption keys), and `area:security` says "Encryption, key management, and related crypto" when no crypto remains in the crate.
- **Issue housekeeping.** [#7](https://github.com/noisypigeon/pigeon-cli/issues/7) and [#19](https://github.com/noisypigeon/pigeon-cli/issues/19) are closed as moot -- both name symbols (`pull_transform/worker.rs`'s upload walk; `download.rs`'s `ANNOUNCE_DOWNLOAD_THRESHOLD_BYTES`) deleted by ADR-0131 and ADR-0133. [#32](https://github.com/noisypigeon/pigeon-cli/issues/32) is retitled and rewritten: it names `import/wizard.rs`, a job renamed by ADR-0110, and the bug it describes turns out to exist in **all five** wizards rather than one.

## Out of scope

- **PR-triggered CI** ([#47](https://github.com/noisypigeon/pigeon-cli/issues/47)). The gate is local `mise run ci` by ADR-0029's design and stays that way; the issue remains open as an accepted, documented gap rather than silently carried. One correction to its text, found while confirming it: `release.yml` runs `fmt-check` and `lint` on push to `main` but **never `cargo test`**, and then publishes to crates.io -- so the gap is not just "no PR check", it is "tests never run server-side at all". Noted in the issue; not acted on here.
- **Unused-dependency detection.** Nothing in the toolchain catches one, which is the structural cause of ADR-0147 shipping two. Filed per ADR-0031 rather than bolted on here, since adding a tool to `mise run ci` is a tooling decision with its own trade-offs.
- **Tightening the `pub mod` back-doors.** `client.rs`'s four dead functions survived ~130 ADRs because a fully-`pub` module path makes `dead_code` blind. Several `pub mod`s under `commands/job/` and `keyring/bucket/` could be `pub(crate)`, which would let the compiler catch the next one instead of an ADR census. Filed per ADR-0031.
- **Issue #63's stderr-substring fragility.** §3 deliberately declines to add a second instance; it does not fix the existing one in `Lsjson::finish`.

## Consequences

- **`minio` is gone, and every S3 access in the crate goes through `core::rclone`.** ADR-0132's consolidation is complete: one abstraction, one config, one place where subprocess scaffolding lives.
- **`PIGEON_SECRET_<ALIAS>` now reaches bucket-configs** for the first time (§1). A deployment that set the variable while relying on `keyring.toml`'s value will newly use the variable. Intended per ADR-0087, but a real behaviour change on upgrade.
- **`keyring add bucket`/`modify bucket` require `rclone` on `PATH`.** `pigeon keyring` was previously usable without it; `pigeon keyring add email` and every other keyring subcommand still are.
- **The verify-before-save error is less specific** (§3): `failed to verify bucket 'x': <rclone stderr>` rather than `bucket 'x' does not exist`. The guarantee is unchanged; a typo is still refused before anything is saved.
- **A rerun re-uploads identical run artifacts** rather than ETag-comparing them first (§2). Unreachable in practice, since ADR-0100 gives every run a fresh prefix.
- **`client.rs`'s four dead `pub` functions are finally gone.** The lesson is structural, not incidental: a fully-`pub` module path disables `dead_code`, so this crate's otherwise-reliable "clippy proves the census" discipline has one blind spot, and that is where dead code accumulated for ~130 ADRs. Filed as out of scope above.

## Amends

**ADR-0009** and **ADR-0010** (the bucket-verify step's mechanism, and the precision of its error -- §3), **ADR-0017** (its `list-buckets` removal left a function "kept for reuse" that is now deleted unused), **ADR-0087** (its `PIGEON_SECRET_<ALIAS>` override now actually reaches bucket-configs -- §1), **ADR-0089** (the streaming `UploadBody::Path` upload path and its multipart ETag recipe are deleted with `client.rs`), **ADR-0091** (`needs_hash`'s skip-the-hash optimisation goes with it), **ADR-0100** (report artifacts move from `minio` to `CopyTo`, and the secret leaves that path -- §2), **ADR-0132** (`RcloneCommand` gains `config_path` -- §3), **ADR-0136** (the generated config now resolves secrets through ADR-0087's override), **ADR-0147** (its "explicitly not dead" tier for `minio`/`md5`/`client.rs` is resolved, and its missed `SKILL.md` mention fixed -- §5).

Draws on without amending: ADR-0029 (dev cycle), ADR-0031 (out-of-scope issues), ADR-0133 (`deduplicate`'s precedent for this same migration), ADR-0135 (the `0600` trust model the probe config inherits), ADR-0140 (`missing_ok`, and the two-section config shape §3 must not drift from), ADR-0145 (`check_available`/`prepare_rclone`).

Each amended ADR gets a short forward-pointing "Amendment" section, per the pattern ADR-0101 already carries.

## Verification

- `mise run ci` clean (fmt-check + lint + test); `cargo clippy --all-targets --all-features -- -D warnings` clean **after** §4's deletion -- the mechanical check that §2's dead-`resolve_secret` pruning was complete.
- **Dependency removal asserted, not assumed.** `grep -rn 'minio\|md5::' src/` returns nothing and `cargo build` succeeds with both removed *before* the manifest edit is committed. ADR-0147 shipped two unused dependencies past an otherwise-clean gate because nothing in the toolchain checks; this ADR does the check by hand and says so.
- **`PIGEON_SECRET_<ALIAS>` end to end**, which is what §1 exists for: a bucket-config whose stored `secret_key` is deliberately wrong, with the correct key in the environment variable, and a real `pigeon job run rclone copy` against it. It fails before §1 and succeeds after. A unit test asserts the generated config carries the override's value rather than the stored one.
- **`keyring add bucket` against the real endpoint**: an existing bucket is accepted, and a nonexistent one is refused. The second half is what settles §3's open question about `no_check_bucket`. Confirm no `rclone-probe-*.conf` survives either outcome, and that an emptied `PATH` fails naming the missing `rclone` binary.
- `pigeon --help` and `cargo metadata` show the corrected description (§5); the `analyze-job-run` runbook no longer contradicts itself on `pigeon_upload_attempts_total`.
- Per ADR-0029: branch `adr-0148-drop-minio`, `type(adr-0148):` commits, local gate, `gh pr create` with a condensed body, one `CHANGELOG.md` `[Unreleased]` bullet, one `CLAUDE.md` ADR-index bullet, squash-merge.
