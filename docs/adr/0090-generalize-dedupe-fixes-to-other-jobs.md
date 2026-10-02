# ADR-0090: Generalize ADR-0088/0089's dedupe fixes to every other job

- **Author**: Willow Finch ([@noisypigeon](https://github.com/noisypigeon)).
- **Date**: 2026-10-02.
- **Status**: Accepted.

## Context

ADR-0088 (CPU-aware concurrency default + `tokio::task::spawn_blocking` for
CPU-bound inline calls) and ADR-0089 (`--upload-only` resume flag + scoping
in-memory dedup state to drop before the upload phase; the streaming-upload
fix itself already lives in the *shared* `upload.rs`/`client.rs` modules,
so it already benefits every job) were built against `dedupe` alone. All
five other job types were audited for the same gaps by reading their
actual code, not by assumption:

| Job | Flat-4 default on CPU-bound work? | Inline blocking CPU calls? | State held through upload? | Resumability gap? |
|---|---|---|---|---|
| `pull-transform` | Yes (generic shared `ConcurrencyInput`) | Yes — `archive::expand_to_dir`, 3x `download::sha256_file`, `process_document_or_other`, `media::exif_date` | Yes — `dedup`/`placed_keys` alive through the upload call | Yes, identical shape |
| `sort` | No — genuinely I/O-only, no hashing at all | No CPU-bound calls, only cheap `fs::`/rename syscalls | Yes — `downloaded: Vec<DownloadedFile>` alive through upload, unused afterward | Yes, identical shape |
| `email-sync` / `email-pull` | Yes, but IMAP-I/O-dominated — flat 4 is defensible here | Yes, but only on small, bounded "email-sized" data (`md5::compute`, `htmd::convert`) | No — already correctly scoped per-identity | Yes, identical shape, but structurally multi-identity |
| `decrypt-files` | Yes (generic shared) | Yes — `fs::read`→`decrypt`→`fs::write`, CPU-bound AES-256-GCM-SIV | N/A — no upload phase at all | N/A — no upload phase |

**A real, pre-existing, unrelated bug found while reading `pull-transform`'s
upload call**: unlike `dedupe`/`sort` (which scope their upload walk to a
separate `result/` subdirectory, deliberately excluding `.staging/`),
`pull-transform` never adopted that split — it places files directly under
`local_output` and calls `pending_upload_tasks(local_output, local_output,
local_output, ...)`. Since `core::data::collect_files` walks every file
including dotfiles, `.processed`/`.content-hashes`/`.uploaded` are literal
upload candidates there. **Not fixed here** — out of scope, pre-existing
and unrelated to concurrency/upload-only; `--upload-only` for
`pull-transform` reuses the exact same call shape so it doesn't regress or
diverge from the existing (if imperfect) normal-path behavior. Filed via
`mise run adr-issue`.

**A real bug caught by a CLI test while implementing this ADR**: the first
`email-sync --upload-only` draft fetched each selected identity's IMAP
secret from the OS keychain before checking whether that identity even had
a completed local run — directly contradicting this ADR's own stated goal
("skips... the per-identity IMAP credentials they'd otherwise need") and
breaking in any environment without a real keychain entry for that alias.
Fixed by never doing a secret lookup for `--upload-only`'s `IdentityContext`
construction at all (`IdentityContext.secret` is provably unused on this
code path — no IMAP connection ever happens — so it's populated with an
empty placeholder instead).

## Decision

Four fix categories, applied only where the per-job audit above actually
supports it — not uniformly, since blind uniformity would be wrong (e.g.
forcing a CPU-core-based concurrency default onto an IMAP-bound job).

### 1. CPU-aware concurrency default — `pull-transform`, `decrypt-files`

Three jobs now need the same `available_parallelism()`-based default
(`dedupe`, and these two) — per this codebase's "duplicate until the third
consumer, then hoist" precedent, the pure helper moved into
`shared_wizard.rs`: `pub(crate) fn default_concurrency() -> usize`
(originally local to `dedupe/wizard.rs`). A new `shared_wizard::
CpuConcurrencyInput` struct wraps it with the same shape as the existing
flat-4 `ConcurrencyInput` (kept unchanged, still used by `sort` and as the
base for `email-sync`/`email-pull`'s own estimate-table variants).
`dedupe`, `pull_transform`, and `decrypt_files`'s wizards all now import
`CpuConcurrencyInput` in place of a local or generic flat-4 input.
`--concurrency <N>` and non-interactive behavior are unchanged everywhere.

`sort` and `email-sync`/`email-pull` are deliberately **not** touched:
`sort` has no CPU-bound work to justify it; the email jobs are IMAP-I/O-
bound, and a cores-based default would be actively wrong there, not just
unnecessary.

### 2. `tokio::task::spawn_blocking` for CPU-bound inline calls — `pull-transform`, `decrypt-files`

Same mechanical pattern ADR-0088 applied to dedupe's `sha256_file`/
`expand_to_dir`:
- `pull_transform/worker.rs`: `archive::expand_to_dir`'s call site, all
  three `download::sha256_file` call sites (via a new shared
  `hash_file_blocking` helper), the `process_document_or_other` call site,
  and the `media::exif_date` call inside `process_media` all now run via
  `spawn_blocking`. `process_item`'s `counter`/`extracted_bytes` parameters
  changed from `&AtomicU64` to `&Arc<AtomicU64>` (same ADR-0088 change) so
  they can be `Arc::clone`d into the blocking closures.
- `decrypt_files/worker.rs`: `decrypt_one`'s `fs::read` → `encryptor.decrypt`
  → `fs::write` sequence now runs inside one `spawn_blocking` closure via a
  new `decrypt_one_blocking` helper. `Aes256GcmSivEncryptor` gained
  `#[derive(Clone)]` (cheap — its inner `AesGcmSiv` cipher already derives
  `Clone`) so an owned copy can move into the 'static closure.

`sort`'s inline calls are cheap filesystem syscalls (`create_dir_all`,
`rename`), not CPU-bound hashing/decompression — explicitly excluded, same
reasoning dedupe's own untouched `next_scratch_path`/`append_checkpoint`
calls already establish as precedent. `email-sync`/`email-pull`'s inline
`md5::compute`/`htmd::convert` calls are excluded too: per-call payload is
bounded/small ("email-sized" per ADR-0025/0089's own framing), so the
blocking duration is negligible next to IMAP I/O already dominating that
batch, and `spawn_blocking`'s own task-handoff overhead would likely exceed
the savings for such small payloads.

### 3. Scope in-memory state to drop before the upload phase — `pull-transform`, `sort`

Same block-expression pattern ADR-0089 applied to dedupe's
`dedup_index`/`merge_records`/`placed_keys`:
- `pull_transform/worker.rs::run_pull_transform_job`: the `ContentIndex::
  load` → `dedup::place_files` → `placed_keys`-filter → checkpoint-append
  sequence is wrapped in a block evaluating to just the placement summary,
  so `dedup`/`placed_keys` drop before the upload call.
- `sort/worker.rs::run_sort_job`: the download-collection →
  `downloaded.sort_by` → placement loop is wrapped in a block evaluating to
  the placement/failure counts, so `downloaded` drops before the upload
  call — its entries are never referenced again after placement, since
  `pending_upload_tasks` independently re-walks the result directory from
  disk.

`email-sync`/`email-pull` need no change — confirmed already correctly
scoped per-identity. `decrypt-files` has no upload phase.

### 4. `--upload-only` resume flag — `pull-transform`, `sort`, `email-sync`, `email-pull`

Same shape as dedupe's (ADR-0089 §1: skip gather/fetch, validate a prior
run's on-disk state, resolve the remote + concurrency + confirm, call a
shared `upload_result`-style helper), adapted per job's actual layout:

- **`pull-transform`**: nearly a direct copy of dedupe's
  `dispatch_upload_only`/`run_upload_only`/`upload_result` pattern, with
  two adjustments for its different layout: preflight checks
  `local_output/.processed` (not `.staging/.processed`) exists and that
  `local_output` holds at least one placed-content subdirectory (an
  `is_dir()` entry other than `.staging`); the upload call reuses the exact
  same `pending_upload_tasks(local_output, local_output, local_output,
  ...)` shape the normal path already uses (see Context). Also resolves
  `EncryptionKeyInput` in the upload-only branch (pull-transform supports
  encryption; dedupe never does). The mandatory `ffmpeg`/`ffprobe`
  availability check now runs *after* the `--upload-only` early-return,
  since that mode never recodes anything and shouldn't require ffmpeg on
  `PATH` at all.
- **`sort`**: same shape as dedupe's (`.staging/.processed` + `result/` —
  sort's layout already matches dedupe's exactly). Remote resolution
  reuses sort's own mandatory `RemoteOutputInput` (not `UploadTargetInput`
  — sort's upload was already mandatory before `--upload-only` existed).
- **`email-sync` / `email-pull`**: the one genuinely new shape, since these
  are multi-identity (`local_output/<alias>/staging/`,
  `local_output/<alias>/result/` per identity). `--upload-only` reuses the
  normal path's own `IdentitiesInput` for identity selection; for each
  selected identity, a per-identity preflight check
  (`identity_has_completed_run`: does `output_dir` exist and hold at least
  one entry) skips — with a printed warning, not a hard failure — any
  identity whose local state isn't complete, rather than aborting the
  whole multi-identity command over one identity. Upload tasks are
  accumulated across every identity that passes preflight into one shared
  `run_upload_phase` call, mirroring how the normal path already
  accumulates tasks across its per-identity loop before one shared upload
  call — the per-identity upload-task-building step itself is hoisted into
  a small `identity_upload_tasks` helper both the normal loop and
  `run_upload_only` call, so there's one code path. Crucially, this mode
  never looks up an IMAP secret for any identity (see Context) —
  `IdentityContext.secret` gets an empty placeholder, since nothing on this
  path ever connects to IMAP.

`decrypt-files` has no upload phase — `--upload-only` doesn't apply, no
flag added.

## Out of scope

- **The `pull-transform` dotfile-upload-sweep bug** flagged in Context — ([#7](https://github.com/noisypigeon/pigeon-cli/issues/7))
  real, pre-existing, unrelated to this ADR's actual ask. Filed via
  `mise run adr-issue` rather than silently folding a fix in here.
- Any change to `email-sync`/`email-pull`'s concurrency default value or
  their inline hashing/conversion calls — both audited and found correct
  as-is for an I/O-bound job.
- Any change to `sort`'s concurrency default — already appropriate for a
  pure I/O job with no CPU-bound work.

## Verification

- Unit tests added per job: `CpuConcurrencyInput`/`default_concurrency()`
  (`shared_wizard.rs`); each job's `*_preflight_ok`/`identity_has_completed_
  run` helper (missing checkpoint, empty result dir, completed-run happy
  path); CLI integration tests confirming `--upload-only` parses and fails
  fast with a clear message against an incomplete/empty local output for
  `dedupe`, `sort`, `pull-transform`, `email-sync`, `email-pull`.
- `pull_transform::worker`'s existing real-`ffmpeg` test
  (`process_media_recodes_a_real_video_to_mp4`) continues to pass
  unmodified, exercising the new `spawn_blocking`-wrapped `exif_date`/hash
  calls end-to-end.
- `mise run ci` clean across the whole diff.
