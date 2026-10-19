# CLI-0131: remove `pigeon job run pull-transform`

- **Author**: Willow Graysen ([@noisypigeon](https://github.com/noisypigeon)).
- **Date**: 2026-10-12.
- **Status**: Accepted.

## Context

`pigeon job run pull-transform` (CLI-0074, amended by CLI-0075/0076/0077)
recursively pulls a bucket-config via the `minio` crate, expands zips, recodes
media through `ffmpeg`, dates files from EXIF/PDF/OOXML metadata, dedups by
SHA-256, organizes by extension, and optionally encrypts and uploads the
result.

Every capability it still offers is now reachable by composing the jobs that
came after it, as separate manually-verified steps:

- bucket-to-bucket movement → `job run rclone copy` (CLI-0101/0110)
- zip inflation + content dedup + extension organization → `job run
  deduplicate` (CLI-0082/0096)
- media transcoding → `job run transform` (CLI-0112, reworked by
  CLI-0116/0124/0127/0129, hardened by CLI-0130)

`transform` is also the architecture this codebase is converging on — all
storage I/O through `rclone`, raw `remote:path` addressing, deterministic
output names, a per-file pipeline with immediate push and local GC. CLI-0113
already planned to migrate `deduplicate` onto that same pull/local-work/push
template. Keeping `pull-transform` means maintaining a second, `minio`-based
media pipeline with its own wizard, worker loop, checkpoint format, media
module, and test suite, duplicating what two other jobs already do.

This repo has two direct precedents for the same situation: **CLI-0094 removed
`sort`** and **CLI-0103 removed `reduce`**, both jobs superseded by how the
others were actually being used. This ADR mirrors their structure. Per
`CLAUDE.md`'s standing rule, the removal is documented explicitly rather than
left as silent drift.

CLI-0130 deliberately landed first, hardening `transform` against four real
defects (including one that silently lost files), so the surviving job was
proven solid before its predecessor was deleted.

## Decision

Delete `src/commands/job/pull_transform/` and prune every reference. Unlike
`sort` and `reduce`, this is **not** a clean one-directional deletion, so it
takes one preparatory move first.

### 0. Hoist `archive.rs` before deleting anything

`deduplicate` has a real, compiling dependency on `pull_transform::archive`
(CLI-0082 §0): `deduplicate/worker.rs` and `deduplicate/dedup.rs` both import
it, using `MAX_ZIP_DEPTH`, `ExtractedMember`, `ArchiveFailure`,
`expand_to_dir`, and `is_apple_double_basename` across 8 call sites.
`pull_transform/mod.rs` declared `archive` `pub(crate)` specifically for this —
the one intentional exception to that module's otherwise-private surface.
Deleting the directory naively breaks the build.

Moved `pull_transform/archive.rs` → `src/commands/job/archive.rs`, beside the
`download.rs`/`upload.rs` hoists it belongs with. The module is self-contained
(`std` + `zip` only), so this is a file move plus an import repoint; its 8 unit
tests moved with it. `deduplicate`'s two call sites now take
`is_apple_double_basename` straight from `core::data` (its real home since
CLI-0130) rather than through an `archive` re-export shim.

### 1. Delete

- `src/commands/job/pull_transform/{mod,wizard,worker,manifest,media,dedup,date,documents}.rs`
  (~60 unit tests with them).
- `src/commands/job/mod.rs`: removed `pub mod pull_transform;`, added
  `pub(crate) mod archive;`.
- `src/commands/job/commands.rs`: removed `pull_transform` from the job-module
  `use` list and its dispatch match arm.
- `src/commands/job/cli.rs`: removed the `JobType::PullTransform { .. }`
  variant (its clap doc comment plus 14 flags) and its
  `Observable::command_name()` arm (`"job.pull-transform"`). `job_name()`
  needed no change — it derives from `command_name()`, exactly as CLI-0103
  found.
- `src/commands/job/cli.rs`'s `Deduplicate` variant carried **user-facing**
  `--help` text reading "Unlike `pull-transform`, every file is always
  processed…". Rewritten, not merely left to dangle.
- `tests/cli.rs`: removed the 6 `pull-transform` integration tests. Five sat
  together; the sixth
  (`job_run_pull_transform_without_ffmpeg_on_path_fails_fast_with_a_clear_error`)
  sat out of sequence after the `deduplicate` block, so a contiguous block
  delete would have missed it.

### 2. Un-hoists (CLI-0094's `ConcurrencyInput` precedent)

| Item | Consumers after | Action |
|---|---|---|
| `shared_wizard::SourceBucketInput` | **0** | Deleted |
| `shared_wizard::EncryptionKeyInput` | 1 (`email_sync`) | Returned to `email_sync::wizard` |
| `core::data::extension_of` | 1 (`deduplicate`) | Returned to `deduplicate::manifest` |
| `core::data::after_zip_separator` | 1 (`deduplicate`) | Returned with it (`extension_of` calls it) |
| `commands/job/download.rs` | **2** | Stays — see §3 |

`SourceBucketInput`'s deletion is mandatory, not stylistic: at zero consumers
it is dead code, and `mise run lint` runs `clippy -D warnings`.

**A recorded fact needs correcting.** CLI-0103 states `SourceBucketInput` had
"2 consumers (`deduplicate`, `pull_transform`)" and that no un-hoisting was
warranted. CLI-0109 silently invalidated that when it made `deduplicate`
multi-source with its own local input — `deduplicate/wizard.rs`'s own doc
comment says so outright ("which stays single-alias for its other consumer,
`pull_transform::wizard`"). It has had exactly **one** consumer since CLI-0109,
so this removal takes it to zero. Noting the correction here rather than
leaving CLI-0103's count to read as current.

A pleasant side effect: `keyring/store.rs` already referred to
`job::email_sync::wizard::EncryptionKeyInput`, a reference that was stale and
is now correct.

`upload.rs` (3 consumers), `ContentIndex`/`Dedup`/`unique_path` (3),
`sanitize_filename` (4), and `core::retry` (5+) all stay comfortably put.

### 3. Why `download.rs` is the stated exception

The blanket "un-hoist anything that drops to one consumer" rule does **not**
apply here, and collapsing it into `deduplicate` would break the build:

- `transform/placement.rs` uses `download::sha256_hex` (CLI-0112).
- `transform/process_one` and its pre-pull check use
  `download::check_disk_space` (CLI-0130, landed one PR earlier).

So `download.rs` retains two genuine consumers, `deduplicate` and `transform`.
Its module doc comment is updated to say which parts each one uses, so the
hoist rationale is accurate rather than resting on a job that no longer
exists.

### 4. Cargo and Docker

- Removed `kamadak-exif`, `lopdf`, and `quick-xml` — each used *only* by
  `pull_transform`'s `media.rs`/`documents.rs`. Neither `cargo build` nor
  `clippy` detects an unused dependency, so these would have rotted silently.
  `Cargo.lock` shed 223 lines.
- **`zip` stays**: `deduplicate/worker.rs` uses it directly, as does the
  hoisted `archive.rs`.
- **`ffmpeg` stays in `Dockerfile`.** CLI-0087 §52 justified installing it as
  `pull-transform`'s requirement, but `transform` requires `ffmpeg`/`ffprobe`
  independently (`transform/media.rs::check_ffmpeg_available`, asserted by
  `tests/cli.rs`). The Dockerfile line carries no `pull-transform` comment, so
  no change was needed — flagged explicitly because a naive "ffmpeg was only
  for `pull-transform`" reading would silently break `transform` in Docker.

### 5. Documentation

Preserving history, updating only live claims — CLI-0094/0103/0086's
distinction.

- **Preserved untouched**: the 44 other ADRs that mention `pull-transform` as
  one of several jobs they touched at the time, `docs/report/0001-*` (a real
  run analysis whose filename encodes the job name), and all 12 `CHANGELOG.md`
  mentions — an accurate record of what shipped, not a live inventory.
- `adr/CLI-0074`/`0075`/`0076`/`0077`: `Status` marked
  `Reversed by CLI-0131`; bodies left intact as the record of why the job
  existed and how it worked.
- `CLAUDE.md`: reversal notes on those four digest bullets, plus new bullets
  for CLI-0130 and this ADR. CLI-0081/0082/0090/0091/0098/0099/0103/0115's
  bullets left alone — accurate history of what those ADRs did.
- `README.md`: a live, copy-pasteable `pigeon job run pull-transform …`
  example in the observability section, swapped to `transform`. Unlike
  CLI-0094/0103, which both recorded "README: no change", this removal *does*
  touch it; the `## Commands` section never listed the job.
- `.claude/skills/analyze-job-run/SKILL.md`: updated the `jq` worked example
  and module-path example, retargeted two current-state guidance mentions, and
  removed the `job.pull-transform` `command_name()` entry, its step-vocabulary
  entry, and its completion-summary format string — the same three deletions
  CLI-0094 made for `job.sort` and CLI-0103 for `job.reduce`. The CLI-0076
  SIGKILL anecdote is kept as history, marked as referring to a removed job.
- `src/observability/metrics.rs`: `record_phase`'s doc comment used
  `pull_transform`'s `recoded`/`fallback` as its example of outcome naming.
  CLI-0094 swapped this exact comment when `sort` went; swapped again, to
  `transform`'s `transcoded`/`copied_through`.
- `src/commands/mod.rs`: "each of the six job wizards" — a live count that
  would have become wrong; made count-free rather than re-pinned to a number
  that will drift again.
- `src/commands/job/report_upload.rs`: its generic `write_summary_report` unit
  test used `"pull-transform"` as an arbitrary job-name fixture. CLI-0103 set
  that value when it removed `reduce`; swapped again, to `email-pull`.
- ~25 doc comments naming `pull_transform` as a surviving hoist consumer or
  cross-reference were pruned or retargeted (same treatment CLI-0103 gave
  `import/wizard.rs`'s dead `reduce::wizard::LocalOutputInput` clause). The 8
  mentions that remain are all explicit historical statements
  ("the since-removed `pull_transform::media` defaulted to…"), deliberately
  kept.

## Out of scope

- Any replacement for `pull-transform`'s combined behavior. The decision is to
  compose `rclone copy` → `deduplicate` → `transform` as separate,
  manually-verified steps.
- Migrating `deduplicate` onto `transform`'s rclone-based template (CLI-0113's
  design, still unimplemented). Unaffected by this removal.

## Consequences

- `pigeon` goes from **7 job types to 6** (`email-sync`, `email-pull`,
  `decrypt-files`, `deduplicate`, `rclone`, `transform` — counting `rclone`
  as one job type with two actions, the same convention CLI-0103 used);
  upload-capable jobs from **4 to 3** (`email-sync`, `email-pull`,
  `deduplicate`).
- **Client-side encryption for file data is gone, with no replacement.**
  `pull-transform` was the only non-email job offering it (CLI-0025/0027);
  `deduplicate` uploads unencrypted and `transform` has no encryption path at
  all. `rclone crypt` remotes appear nowhere in this repo, so this ADR
  explicitly does *not* claim them as a substitute — doing so would be an
  undocumented assumption dressed as a decision. `job run decrypt-files`
  remains for existing `.enc` archives, and `email-sync` keeps its own
  encrypted-upload path. This is an accepted loss, not an oversight.
- Also lost, with no direct equivalent: EXIF/PDF/OOXML date extraction and
  date-based file organization; per-run selectable output formats
  (`--image-format`/`--video-format`/`--audio-format` — `transform` is
  hardcoded to jpg/mp4 with a `--video-quality` CRF menu); and recode
  fallback-to-original (see [#62](https://github.com/noisypigeon/pigeon-cli/issues/62)).
  Zip expansion and content dedup survive in `deduplicate`.
- `commands/job/archive.rs` is now a shared module with one consumer
  (`deduplicate`). Left shared rather than folded into `deduplicate`: it is a
  self-contained, well-tested unit, and folding it in would be churn for its
  own sake. Noted so a future reader knows it was considered.

## Verification

- `mise run ci` clean: 380 lib tests (was 440 — ~60 deleted with the job) and
  53 integration tests (was 59 — the 6 removed above). `archive.rs`'s 8 tests
  and the 4 relocated `extension_of` tests confirmed still present and passing
  in their new homes.
- `cargo build` succeeds with the three dependencies removed.
- `pigeon job run --help` no longer lists `pull-transform`;
  `pigeon job run pull-transform --help` fails as an unrecognized subcommand.
- `pigeon job run deduplicate --help` no longer references the deleted job.
- Repo-wide grep for `pull_transform`/`pull-transform` leaves only the
  intentionally-preserved historical prose listed in §5.
