# ADR-0147: remove `email-sync` and `email-pull`, and client-side encryption with them

- **Author**: Willow Graysen ([@noisypigeon](https://github.com/noisypigeon)).
- **Date**: 2026-10-13.
- **Status**: Accepted.

## Context

ADR-0146 added `pigeon job run email import` *alongside* `email-sync` (ADR-0007/0021) and `email-pull` (ADR-0081), deliberately leaving both in place so `main` was never in a transitional state. That was the same land-the-successor-first sequencing ADR-0131 used -- ADR-0130 hardened `transform` before `pull-transform` was deleted, so the surviving job was proven solid first.

The successor has landed, so the two predecessors go. Every capability either still offered that is wanted is reachable from `email import`: raw `.eml` delivery, unpacked attachments, resume, and upload to a configured bucket. What is *not* reachable is enumerated in `## Consequences` as lost rather than quietly dropped.

ADR-0146 §10 pre-recorded the forced-deletion census for this ADR, "so it is recorded here rather than left for that implementation to rediscover." Re-tracing it against the code rather than trusting it found **four errors**, each of which would have broken the build or silently over-pruned. Correcting them here rather than leaving §10 to read as complete follows ADR-0131's own "a recorded fact needs correcting" precedent:

1. **`deduplicate` is a surviving consumer of `email_sync::sink`.** §10 says the seven `sink` functions move into `email_import/` and that "nothing else in either directory has a surviving consumer". That holds for six of them. But `sink::new_progress_bar` is called from `deduplicate/worker.rs:409` and `deduplicate/dedup.rs:108`, both reaching it via `use crate::commands::job::email_sync::sink` -- the same `pub mod` back-door ADR-0146 §2 noted `email_pull` already used. Relocating it into `email_import/` would break the build *and* point the dependency the wrong way, since `deduplicate` has nothing to do with email. Handled in §0.
2. **`core/data.rs` loses five `pub(crate)` items**, none listed in §10: **two of ADR-0023's four core traits** -- `Transform`, whose only implementor in this crate's entire history was `EmailTransform`, and `Dedup`, which keeps an implementor but turns out to have no production caller at all -- plus `amend_frontmatter_for_duplicate`, `rewrite_attachment_reference`, and `yaml_quote`. `Dedup`'s removal cascades into three `ContentIndex` methods. Detailed in §3.
3. **`client::UploadBody::Bytes` falls to zero producers**, unlisted in §10 -- the variant that existed solely for ADR-0025's whole-buffer encryption, so it goes with it. (A first pass at this census guessed `UploadOutcome` here instead; that one survives, because `report_upload.rs:164` returns it through an `if let Err(..)` without ever naming its variants. Recorded because the near-miss is instructive: reading a type as dead from its *neighbours* rather than from the compiler is exactly what `clippy -D warnings` is the check against.)
4. **`md5` is not pruned.** §10 lists it among the dependencies this ADR drops, but `keyring/bucket/client.rs` uses it for ETag comparison and that file survives here. It is a candidate for ADR-0148, not this ADR. (`bytes`, which §10 did *not* list, prunes here instead -- see §3.)

Two further things §10 was simply silent on, both verified rather than assumed:

- **`core::job::Job` survives with five implementors** (`ImportJob`, `DeduplicateJob`, `TransformJob`, `RcloneCopyJob`, `RcloneDeleteJob`), so the trait is not dead. But `src/core/job.rs:2` and `:11` still name `EmailSyncJob` as its one real implementor -- a doc comment ADR-0134 already corrected once, which would now name a deleted type.
- **Two metrics cease to exist crate-wide** (§4): `pigeon_upload_attempts_total` and `pigeon_upload_duration_seconds` are emitted only from `upload.rs`. `pigeon_upload_bytes_total` and `pigeon_upload_outcomes_total` **survive**, because `core::rclone` emits both for every rclone-backed job.

## Decision

### 0. Split `sink` three ways before deleting anything

Unlike `sort` (ADR-0094) and `reduce` (ADR-0103), and like `pull-transform` (ADR-0131), this is **not** a clean one-directional deletion. It takes one preparatory move, and `sink`'s contents divide by what each part actually *is* rather than by which directory it currently lives in:

| Item | Consumers after | Action |
|---|---|---|
| `new_progress_bar` | **2** -- `email_import`, `deduplicate` (×2 call sites) | Hoisted to a new `src/commands/job/progress.rs` |
| `fetch_uids`, `sanitize_mailbox_path`, `missing_uids`, `is_stale`, `read_uidvalidity`, `write_uidvalidity` | 1 -- `email_import` | Moved into `src/commands/job/email_import/sink.rs` |
| `run`, `SinkSummary` | **0** | Deleted outright |
| `on_disk_uids`, `clear_eml_files` | **0** | Deleted, with their 3 tests |

`new_progress_bar` builds an `indicatif` bar with a shared template. There is nothing IMAP about it, and it only ever lived in `sink.rs` because that is where the first progress bar happened to be needed (ADR-0013). At two consumers a shared module is the right home -- the same stated exception ADR-0131 §3 made for `download.rs`, and naming the new module for what it holds follows ADR-0133 §9's `download.rs` -> `local.rs` rename.

The six IMAP/mailbox functions have exactly one consumer and belong to it. Nine of `sink.rs`'s twelve tests cover precisely those six and move with them (ADR-0131's "its 8 unit tests moved with it"); the remaining three cover `on_disk_uids`/`clear_eml_files` and are deleted with them.

`sink::run` and `SinkSummary` are the `pigeon email sync --debug sink` entry point from ADR-0007 §4, dead since ADR-0021 replaced that CLI and surviving `clippy -D warnings` only because `pub mod sink` is reachable from `lib.rs`. ADR-0146's Context already recorded this; here it is finally deleted.

**A sixth relocation item §10 does not list.** `email_sync::DEFAULT_MAX_CONNECTIONS_PER_IDENTITY` is referenced from live `email_import` code -- `email_import/mod.rs:311`, inside a `const { assert!(...) }` block -- so under `--all-targets` it is a compile error, not a stale test fixture. That assertion existed to pin `email_import`'s default of 2 as *deliberately lower* than email-sync's 6. With the referent gone the comparison has nothing to compare against, so it becomes `assert_eq!(DEFAULT_MAX_CONNECTIONS, 2)` and the intent survives as prose naming 6 as the historical value.

### 1. Delete

| Path | Lines | Unit tests |
|---|---|---|
| `src/commands/job/email_sync/` | 4,744 (7 files) | 73 |
| `src/commands/job/email_pull/` | 2,188 (5 files) | 21 |
| `src/commands/job/upload.rs` | ~580 | the whole `minio` upload phase |
| `src/core/crypto.rs` | 145 | 6 |
| `src/commands/keyring/encryption_key.rs` | 32 | -- |

Plus the CLI surface: the `JobType::EmailSync` and `JobType::EmailPull` variants (`job/cli.rs:26-191`), both `impl Observable` arms (`:631-632`), both dispatch arms (`job/commands.rs:13-57`), and the two `pub mod` declarations.

`upload.rs` goes because it reaches **zero** consumers: `email_sync/worker.rs:10` and `email_pull/worker.rs:13` are its only importers, `deduplicate` having left for `core::rclone` in ADR-0133 and the two other former consumers having been removed by ADR-0131 and ADR-0134. The two `deduplicate` references to it that remain are doc comments explicitly stating that `deduplicate` bypasses it.

### 2. Remove client-side encryption

`email-sync` was the only caller of `Encryptor::encrypt` anywhere in the crate. Leaving a keyring kind that nothing can consume would be worse than removing it -- the same clean sweep ADR-0134 performed when it removed `decrypt-files` rather than preserving its machinery dormant a second time. This **explicitly reverses ADR-0025, ADR-0026, and ADR-0027**.

Removed: `Entry::EncryptionKey` and its three `KeyringEntry` arms; `Store::encryption_keys()`; `Store::prompt_select_encryption_key()`; the `EncryptionKey(_)` arms in the exhaustive matches inside `email_identities()`/`bucket_configs()`; `AddKind::EncryptionKey` (`keyring/cli.rs:84-90`); `add_encryption_key`/`modify_encryption_key` and their dispatch arms, which takes bare `pigeon keyring add`'s `Select` from three items to two; `BucketConfig::encryption_key_alias` along with the `detail()` branch that appended `, encrypts with '...'`; and both the `add_bucket`/`modify_bucket` prompts that set it.

Dependencies pruned: `aes-gcm-siv`, `hkdf`, `hmac` -- all three used only by `core/crypto.rs` -- plus `bytes`, which §3's `UploadBody` collapse frees a PR earlier than planned. **`sha2` and `hex` stay**, both used by `job/local.rs`'s hashing helpers; they are the near-miss worth naming, since a glance at `crypto.rs`'s imports would suggest otherwise. Nothing in the toolchain detects an unused dependency (ADR-0131's own finding), so this is by inspection rather than by lint.

**The one real migration hazard, and the decision it forces.** `Entry` is internally tagged (`#[serde(tag = "kind", rename_all = "kebab-case")]`), and serde errors on an *unknown variant tag* regardless of `deny_unknown_fields`, which appears nowhere in this crate. An existing `kind = "encryption-key"` entry therefore makes `Store::load` fail -- and `Store::load` runs for **every** command, including `pigeon keyring list` and `pigeon keyring delete`. An operator with a configured key would be locked out of the only two commands that could remove it, with no in-CLI path to recovery.

The fix keeps failing loudly but makes the failure actionable: `Store::load` detects that variant name in the parse error and appends remediation naming the file and the block to delete. This is ADR-0145's `load_keyring` precedent, where the answer to a failure that "names neither the real problem nor the path" was a better message rather than tolerance.

Deliberately **not** a `#[serde(other)]` catch-all variant. That would self-heal, but it would also silently swallow a genuinely malformed `kind` -- reversing the fail-loudly posture ADR-0135 chose for exactly this file when it moved secrets into it in plaintext.

Dropping `encryption_key_alias` from `BucketConfig` needs no migration at all: with no `deny_unknown_fields`, a stale line is ignored on load and gone after the next `save`. Stated because the two halves of this removal have genuinely different compatibility stories, and conflating them would make the field look more dangerous than it is.

### 3. Forced deletions -- the consumer census

`clippy --all-targets --all-features -- -D warnings` is a hard gate here, so anything reaching zero consumers must be deleted rather than left behind. Three tiers, following ADR-0133 §8.

**Zero consumers and `pub(crate)`, so a hard lint failure:**

- `shared_wizard::UploadConcurrencyInput`, plus the private `UPLOAD_CONCURRENCY_DEFAULT` only it reads. `email_import` resolves its push parallelism through its own `PushConcurrencyInput` (ADR-0146 §1), so this reaches zero -- the same un-hoisting ADR-0094 did to `ConcurrencyInput` and ADR-0131 to `SourceBucketInput`, and **mandatory rather than stylistic** at zero consumers. `UploadTargetInput` is the near-miss: it drops to one consumer (`deduplicate`) and **stays**, since un-hoisting it would be churn for its own sake.
- `core::data::Transform` **and `core::data::Dedup`** -- two of ADR-0023's four core traits, not one. `Dedup`'s only implementor was `deduplicate`'s `DeduplicateDedup`, whose `check`/`commit` were pass-throughs to `ContentIndex` that **nothing called in production**: `place_and_report` uses the bucket-keyed `check_with_original_key_and_bucket`/`commit_with_key_and_bucket` instead, and the trait methods' only caller was a single test. So the trait, its pass-through impl, and `ContentIndex::{check, commit, commit_with_key}` all go, and that test now asserts against the lookup placement actually performs. Also `amend_frontmatter_for_duplicate`, `rewrite_attachment_reference`, `yaml_quote`, and their tests.
- `crypto::Encryptor` and its `impl` block -- moot in practice, since §2 deletes the file, but recorded because the trait's death is a separate fact from the file's: `.encrypt()`'s only call site in the crate is `upload.rs:267`.
- `client::UploadBody::Bytes`. Context's item 3 predicted `UploadOutcome` here; the census was right that something in `client.rs` falls over and wrong about which item. `UploadOutcome` survives, because `report_upload.rs` returns it through an `if let Err(..)` without naming its variants. What actually dies is the `Bytes` variant, whose own doc comment states it exists because "ADR-0025's AES-256-GCM-SIV encryption is whole-buffer" -- so it goes with the encryption. With one variant left the enum is pointless, so `upload_if_changed` now takes a plain `PathBuf`, `needs_hash` loses the body parameter it only needed to special-case `Bytes`, and **`bytes` prunes here rather than in ADR-0148**.

**Zero consumers but `pub` on a fully-`pub` module path, so exempt from `dead_code`.** Already dead *today*, unchanged by this ADR, and deliberately **not** pruned here so this census is not mistaken for a complete inventory of the file: `client::ObjectEntry`, `client::list_buckets`, `client::list_objects`, `client::download_object_to_file`.

**Explicitly not dead, despite appearances -- recorded so a later reader does not over-prune:**

- **`minio`, `bytes`, `md5`, and `keyring/bucket/client.rs`.** Every *job-level* data path has now left for `core::rclone`, which makes this look prunable, but two consumers remain: `report_upload.rs` (ADR-0100's run artifacts) and `keyring/wizard.rs`'s `bucket_exists` (ADR-0010's verify-before-save). `async-native-tls` stays with them, which is what `Dockerfile:10`'s `openssl-sys` comment rests on. This is the single most likely over-prune on this change, and the exact inverse of the `ffmpeg` trap ADR-0131 §4 flagged.
- `core::data::{Dedup, ContentIndex, unique_path, sanitize_filename, collect_files, is_apple_double_basename}`, all with surviving consumers in `deduplicate`/`transform`/`email_import`.
- `job/local.rs` entirely -- zero references from either deleted directory.
- `keyring/email/{imap_client,provider}.rs` -- `email_import` and `keyring/wizard.rs`'s `verify_login` keep every item alive.
- All four of `observability::metrics`'s helpers, and `core::retry`.

### 4. Metrics

No metrics *code* changes beyond doc comments. `record_phase`/`record_phase_count`/`set_macro_phase` take a plain `&'static str` label, so nothing hardcodes a job name list -- the same finding ADR-0094 and ADR-0103 both recorded. Two real effects are worth stating rather than discovering from a blank panel:

- **`pigeon_upload_attempts_total` and `pigeon_upload_duration_seconds` cease to exist crate-wide.** Both were emitted only from `upload.rs`. Neither appears in any dashboard. This incidentally makes ADR-0102's deferred issue moot -- it was filed because those two metrics were unimplemented for the `import` job specifically; they are now unimplemented everywhere, by removal rather than by omission. Closed with a pointer here, per ADR-0094's "noted for continuity".
- **`pigeon_upload_bytes_total` and `pigeon_upload_outcomes_total` survive**, emitted from `core::rclone::mod.rs` and `core::rclone::command.rs` for every rclone-backed job.

Consequently **`observability/` needs no change**: the Grafana dashboard's only `pigeon_job`-scoped query is the wildcard `{pigeon_job=~".+"}`, its one template variable keys on `instance` rather than `pigeon_job`, and Promtail derives the `pigeon_job` label from each line's `spans[0].command` at ingest, so it tracks `command_name()` automatically. No dashboard, Prometheus, or Promtail config enumerates job names.

### 5. Documentation

- **`README.md`.** The `## Commands` section is rewritten rather than patched. `:109` names `email-sync`, and **`:110` still documents `pigeon job run decrypt-files`, which ADR-0134 removed without touching this file** -- that ADR's `### Documentation` section lists only ADR-0028's Status and the changelog, and unlike ADR-0094/0103 it never recorded a "README: no change" line either. Deleting only the `email-sync` bullet would leave the section listing exactly one command that does not exist and none that do. Also updated: `:70`'s copy-pasteable Docker example (to `job run email import`, which exercises the same `PIGEON_SECRET_<ALIAS>` path it exists to demonstrate), `:105`'s `[email|bucket|encryption-key]`, and `:3-6`'s "syncs, transforms, and optionally encrypts ... and file encryption".
- **`CLAUDE.md:3`.** Its opening sentence claims the CLI "authenticates, syncs, transforms, and optionally encrypts" and that the keyring holds "identities, buckets, and encryption keys". Three of those clauses become false. This is a live claim about current state, not history, so it is edited. Reversal notes go on the bullets for the ADRs this change reverses or supersedes (ADR-0005/0006/0007, 0011, 0012, 0021, 0025/0026/0027, 0081), and ADR-0146's bullet gets a "done in ADR-0147" update. **Every other bullet is left alone** -- accurate history of what those ADRs did, not claims about now, which is the unanimous policy of ADR-0094, ADR-0103, and ADR-0131.
- **`src/cli.rs:48`** reads `/// Run job-orchestrated pipelines (e.g. email-sync)`. This is a clap doc comment, so the removed job's name appears in `pigeon --help` output, and no test asserts on it -- which is exactly why it would dangle unnoticed. Rewritten, not left to dangle (ADR-0131 §1's treatment of the same situation in `deduplicate`'s help text).
- **`.claude/skills/analyze-job-run/SKILL.md`** gets the same three deletions ADR-0094 made for `job.sort`, ADR-0103 for `job.reduce`, and ADR-0131 for `job.pull-transform`: the `command_name()` cheat sheet, the per-job `step` vocabulary, and the completion-summary format string. Plus its worked `jq` example and the `pigeon_upload_attempts_total` paragraph, now describing a metric that no longer exists (§4). **Two pre-existing bugs are swept up in the same pass**, since the file's own rule is "update this cheat sheet in the same PR that changes the logging code": ADR-0134 left `job.decrypt-files` in two places despite claiming a clean repo-wide grep, and ADR-0146 shipped a new job without ever *adding* `job.email-import`.
- **Doc comments naming either job as live fact** are retargeted or made count-free: `core/job.rs:2,11`, `core/data.rs:7,17,190-191` (a live consumer count -- made count-free rather than re-pinned to a number that will drift again, ADR-0131's treatment of `commands/mod.rs`), `observability/resources.rs:8`, `observability/metrics.rs:61,80`, `keyring/email/identity.rs:13,24-25`, `keyring/store.rs:231,240,306`, `shared_wizard.rs:3,6,11,98,215`. Two of these have a history worth noting: `metrics.rs:61` was swapped by ADR-0094 and again by ADR-0131, making this its third swap; and `keyring/store.rs:306` names `job::email_sync::wizard::EncryptionKeyInput`, a reference ADR-0131 had just *fixed* by un-hoisting that input back to `email_sync`.
- **`report_upload.rs`'s test fixtures** use `"email-pull"`/`"email-sync"` as an arbitrary job name. ADR-0103 set that value to `"pull-transform"`, ADR-0131 changed it to `"email-pull"`, and this is its third change -- so it moves to `deduplicate`, a job with no removal ADR pending.
- **`.github/scripts/adr-issue.sh:54`** defines the `area:email-sync` GitHub label as "IMAP sink/transform/sync pipeline". The name now points at a removed job and the description at a pipeline that no longer exists. Existing issues carry the label, so renaming it is a GitHub-side migration rather than a file edit; the name is **kept** and the description corrected to name `email import`. Stated rather than drifted, per ADR-0094's handling of issue-tracker state inside a removal ADR.
- **`CHANGELOG.md`** gets one new `[Unreleased]` bullet and nothing else. Its existing mentions are an accurate record of what shipped (ADR-0094/0103/0131, unanimous).
- `docs/adr/` historical prose stays throughout.

## Out of scope

- **Re-adding a mail-to-Markdown transform in a different shape** -- for example as its own job over already-imported `.eml` files. Not proposed here; the decision is that this capability is not wanted, not that it should be rebuilt elsewhere. A permanent boundary, so no ADR-0031 issue is filed.
- **Any replacement for client-side encryption.** `rclone crypt` remotes appear nowhere in this repo and are explicitly **not** claimed as a substitute -- doing so would be an undocumented assumption dressed as a decision (ADR-0131's phrasing for the same situation).
- **Migrating `report_upload.rs` and `bucket_exists` off `minio`**, which is what would actually let `minio`/`bytes`/`md5`/`keyring/bucket/client.rs` go. Deliberately deferred to ADR-0148: it is a behaviour-changing migration (`HeadBucket`'s three-way answer has no `rclone` equivalent) and does not belong inside a deletion this size.

## Consequences

- **`pigeon` goes from 7 job types to 5** -- `email import`, `deduplicate`, `transform`, and `rclone copy`/`rclone delete`, counting `rclone` as one type with two actions, the convention ADR-0103 and ADR-0131 both used. All five are upload-capable.
- **`.eml`-to-Markdown conversion with frontmatter taxonomy is gone, with no replacement claimed.** ADR-0001 through ADR-0007's entire product thesis -- that mail should become browsable Markdown with namespaced tags -- leaves the crate here. `email import` delivers raw `.eml` and unpacked attachments and nothing else, by design (ADR-0146). An accepted loss, not an oversight.
- **Client-side encryption is gone entirely**, reversing ADR-0025/0026/0027. No data path in the crate encrypts, and the `encryption-key` keyring kind is removed rather than left dormant. Also an accepted loss.
- **Email message and attachment deduplication is gone** (ADR-0012, and ADR-0030's fix to it). Byte-identical duplicates now reach the destination as separate objects; collapsing them is a downstream `job run deduplicate` concern, which is the compose-separate-verified-steps posture ADR-0103 and ADR-0131 already adopted.
- **An operator with a configured encryption key must hand-edit `keyring.toml` once.** §2's error message tells them exactly what to delete, but there is no automated migration, consistent with ADR-0135's own no-shim precedent for this file.
- **`core::data` is down to two of ADR-0023's four traits.** Both `Transform` and `Dedup` go, leaving `Job` and `KeyringEntry`. `Transform` had exactly one implementor across the crate's whole history. `Dedup` kept an implementor but no caller -- `deduplicate` satisfied the trait and then reached past it to `ContentIndex`'s bucket-keyed methods, which is the clearest possible signal that the generalisation never earned its keep. ADR-0023's own Consequences named the risk of introducing traits ahead of a second implementation; this is that bill arriving.
- **Two upload metrics disappear** (§4), neither dashboarded, and ADR-0102's issue about them goes moot.
- `commands/job/progress.rs` joins `archive.rs` and `local.rs` as a small shared module with two consumers. Left shared rather than duplicated into both, for the reason ADR-0131 §3 gave for `download.rs`: it is a self-contained, tested unit and folding it in would be churn.

## Amends

**ADR-0005** and **ADR-0006** (the original `sink`/`transform` commands, whose last surviving descendant goes here), **ADR-0007** (`email sync`, removed), **ADR-0011** (`--output-remote`, removed with the job), **ADR-0012** and **ADR-0030** (email dedup, removed), **ADR-0021** (the job wizard's first job type, removed), **ADR-0023** (four core traits, now three -- §3), **ADR-0025**, **ADR-0026**, and **ADR-0027** (client-side encryption, reversed outright -- §2), **ADR-0032** and **ADR-0034** (the manifest attachment estimate, which had no consumer left after ADR-0146 §7 and now has no code either), **ADR-0081** (`email-pull`, removed), **ADR-0102** (its deferred per-attempt-metrics issue goes moot -- §4), and **ADR-0146** (§10's census, corrected in four places -- Context).

Draws on without amending: ADR-0029 (dev cycle), ADR-0094/0103/0131/0134 (removal precedents, structure, and the un-hoist/census/documentation conventions), ADR-0133 §8 (the three-tier census format), ADR-0135 (the no-migration-shim precedent for `keyring.toml`), ADR-0145 (the actionable-error precedent §2 follows).

Each amended ADR gets a short forward-pointing "Amendment" section, per the pattern ADR-0101 already carries.

## Verification

- `mise run ci` clean (fmt-check + lint + test), with the arithmetic stated: **lib tests 490 -> 368** and **integration tests 71 -> 65**.

  Lib: 109 removed with the deleted files (73 `email_sync`, 21 `email_pull`, 9 `upload.rs`, 6 `core/crypto.rs`) and 22 more from the files §3 pruned (17 in `core/data.rs`, 2 in `shared_wizard.rs`, 2 in `keyring/store.rs` net of the two added in §2, 1 in `client.rs`), against 9 added back -- **9 of `sink.rs`'s 12 tests were relocated rather than removed** and are confirmed passing in `email_import/sink.rs`. Integration: the 10 removed, against 4 added (a subcommand-rejection test for both removed spellings, §2's stale-entry error, its stale-field companion, and a `keyring add` surface assertion).
- `cargo clippy --all-targets --all-features -- -D warnings` clean **specifically after §3's deletions** -- the mechanical check that the census was complete, which is why it is run before any hand-audit.
- `pigeon job run --help` lists four entries -- `email`, `deduplicate`, `rclone`, `transform` -- covering the five remaining job types, `email` and `rclone` each being a group with actions under it (the counting convention ADR-0103 and ADR-0131 both used). Neither old spelling appears, and `pigeon job run email-sync --help` / `email-pull --help` both fail with `unrecognized subcommand`. `pigeon keyring add --help` offers exactly `email` and `bucket`.
- A `keyring.toml` carrying a `kind = "encryption-key"` entry fails with a message naming both the file and the block to delete -- asserted in `tests/cli.rs`, since this failure path otherwise locks the operator out of every keyring command (§2).
- A `keyring.toml` carrying a stale `encryption_key_alias` on a bucket entry loads without complaint, and the field is absent after the next `keyring modify`.
- `job_run_help_lists_the_email_group` is tightened: it previously asserted only that `job run --help` contains `"email"`, which `"email-sync"` already satisfied, so it could not distinguish the `email` group from its flat siblings. It now also asserts both old spellings are gone.
- `deduplicate`'s two `new_progress_bar` call sites compile against `job::progress`, and `email_import`'s six `sink` call sites against its own `sink` module.
- Repo-wide grep for `email_sync`/`email-sync`/`email_pull`/`email-pull` leaves only the historical prose §5 enumerates. The full job names are grepped, not `email`: `email_import` shares the prefix, and "email" appears throughout a crate whose subject is mail.
