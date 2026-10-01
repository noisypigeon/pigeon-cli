# ADR-0081: `pigeon job run email-pull`

- **Author**: Willow Finch ([@noisypigeon](https://github.com/noisypigeon)).
- **Date**: 2026-09-30.
- **Status**: Accepted.

## Context

`email-sync` (ADR-0007/0021) always converts fetched mail into Markdown +
frontmatter and deletes the raw `.eml` once verified (ADR-0007 reversed
ADR-0001's permanent-raw-archive default on purpose). There's no job today
that keeps the raw `.eml` and unpacked attachment files themselves as the
deliverable — useful when the raw message is what's wanted, not a
Markdown/taxonomy rendering of it. The want: a new sibling job, `pigeon job
run email-pull`, that reuses `email-sync`'s wizard shape, pulls raw `.eml`
files and unpacks their attachments as separate files, deduplicates
attachments by content, never encrypts on upload, and optionally uploads the
result to a bucket-config.

This closely follows the `Job`/`WizardInput`/wizard-orchestration shape
`email-sync` (ADR-0021/0023/0024/0025) already established, and reuses the
generic dedup/upload/wizard infrastructure ADR-0020 and ADR-0074 already
extracted — the goal is to extend that family, not re-invent it.

A few things worth calling out up front, found by reading the current
implementation directly rather than assuming from prior ADR text:

- `commands/job/email_sync/sink.rs` still has the ADR-0005-era raw-`.eml`
  fetch pathway (`fetch_uids`, `new_progress_bar`, `sanitize_mailbox_path`,
  `missing_uids`, the `UIDVALIDITY` helpers, `on_disk_uids`) — orphaned from
  the CLI surface since ADR-0021 removed `pigeon email sync --debug sink`,
  but still `pub(crate)` and directly reusable.
- `commands/job/email_sync/manifest.rs`'s `ManifestEntry`, `pull_manifest`
  (with ADR-0065's bisect-on-failure), `save_manifest`, `Batch`, and
  `split_into_batches` are pure IMAP/batching mechanics with no Markdown
  coupling at all — directly reusable as-is. Only `CheckpointEntry` and its
  `append_checkpoint`/`load_checkpoint`/`done_uids` helpers are
  Markdown-shaped (`md_staged_relpath`, `desired_md_name`, `mailbox_tag`)
  and need a pull-specific equivalent.
- `email_sync::worker.rs`'s `connect_with_retry` is likewise IMAP-generic,
  but `run_worker`/`process_batch_on_session` are concretely typed around
  `Batch`/`CheckpointEntry` and directly construct `EmailTransform` — these
  need their own copy for `email-pull`, since only the body of the per-UID
  step actually differs (extract attachments, don't render Markdown).
- **ADR-0074 (`pull-transform`) already solved the exact dedup problem this
  job would otherwise hit.** `email-sync`'s whole-message dedup
  (`amend_frontmatter_for_duplicate`/`also-in:`, ADR-0012) only works
  because Markdown output has a frontmatter block to rewrite in place — a
  raw `.eml` has no such block, and there's no cheap, safe equivalent.
  `pull_transform::dedup` sidesteps this entirely by never rewriting a
  canonical file: a content-hash duplicate is simply discarded, a unique
  file is simply placed, via a plain `core::data::ContentIndex` with no
  back-reference to maintain. Scoping `email-pull`'s dedup to
  **attachments only** (not whole messages) sidesteps the same problem the
  same way: raw `.eml` files are never deduped against each other — each
  fetched message is kept, self-contained, exactly as the server sent it —
  only the separately-extracted attachment copies are content-deduped.

## Decision

### 0. Command surface

`JobType::EmailPull { identities: Option<Vec<String>>, local_output:
Option<PathBuf>, remote_output: Option<String>, concurrency:
Option<usize>, max_connections_per_identity: Option<usize>, yes: bool }`,
added to `commands/job/cli.rs` alongside `EmailSync`/`DecryptFiles`/
`PullTransform`. Modeled directly on `EmailSync`'s flags **minus
`encryption_key`** — `email-pull` never offers encryption, at all, on
purpose (see §4). `Observable` arm: `"job.email-pull"` (ADR-0073). One new
match arm in `commands/job/commands.rs` delegating to
`email_pull::wizard::dispatch(...)`.

### 1. New job scaffold

- `src/commands/job/email_pull/{mod,wizard,manifest,worker,dedup}.rs`
  (mirrors `email_sync`'s module shape, ADR-0008).
- `EmailPullJob { identities: Vec<IdentityContext> }`-shaped struct
  implementing `core::job::Job` (`Plan = Vec<Vec<PendingMailbox>>`,
  `Summary = EmailPullSummary`), same shape as `EmailSyncJob`.

### 2. Wizard flow (mirrors ADR-0021's ordering)

`IdentitiesInput`/`LocalOutputInput` (own local copies, same shape as
`email_sync::wizard`'s — required identities, defaulted local-output dir) →
`job.gather()` pulls the manifest and prints a pending-message summary table
→ `UploadTargetInput` (shared, `commands/job/shared_wizard.rs`) → **no
`EncryptionKeyInput` at all** (§4) → own `ConcurrencyInput` with the same
message-count time-estimate table `email_sync`'s local one already prints
(comparable IMAP-bound throughput profile, so the existing heuristic
applies) → `ConfirmInput` (shared) → `job.run(plan, concurrency)`.

### 3. Output layout

- **Raw messages**: `<local-output>/<identity-email-sanitized>/<sanitized-mailbox>/<uid>.eml`
  — the same layout `sink::fetch_uids` already produces, written straight to
  its **final** location. No staging/rename step is needed for the `.eml`
  itself: `(mailbox, uid)` is already unique per IMAP's own guarantees, so
  there's no dedup contention on it the way there is on a shared,
  content-hash-deduped tree.
- **Attachments**: `<identity-email-sanitized>/attachments/<sanitized-name>`,
  flat, content-deduped. Extracted during the concurrent fetch phase into a
  UID-keyed scratch location first (avoiding a `unique_path`/`ContentIndex`
  race across concurrent workers — the same reason `email_sync`'s and
  `pull_transform`'s dedup/placement passes are single-threaded, ADR-0021's
  addendum), then moved to a final path (or discarded as a duplicate) by one
  single-threaded placement pass once fetching finishes. Keyed by a new
  `.attachment-hashes` `ContentIndex` file — same filename convention as
  `email_sync::transform::ATTACHMENT_HASHES_FILE`, MD5 like email-sync's
  (not SHA — there's no stated reason to diverge here the way
  `pull_transform`'s explicit SHA ask did).
- No `.message-hashes` file and no `also-in:`/frontmatter concept exists for
  this job at all — a fetched `.eml` is immutable once written, never
  rewritten.

### 4. No encryption, ever

`email-pull` never constructs an `Aes256GcmSivEncryptor` and never offers
`--encryption-key` or any encrypt-this-upload prompt. This is a deliberate,
stated scoping decision (the job's whole point is a plain raw-file pull),
not an oversight or a gap to casually revisit later — wiring in the
`EncryptionKeyInput`/`Encryptor` machinery and then just not using it would
invite scope creep that contradicts the ask. If encrypted raw-pull uploads
are ever wanted, that's a new ADR amending this one explicitly, not a quiet
addition.

### 5. Fetch + extract phase (concurrent, new `email_pull/worker.rs`)

Reuses `email_sync::{manifest::{pull_manifest, ManifestEntry, save_manifest,
Batch, split_into_batches}, sink::{fetch_uids, sanitize_mailbox_path,
new_progress_bar, missing_uids, is_stale, read_uidvalidity,
write_uidvalidity, clear_eml_files, on_disk_uids}, worker::connect_with_retry}`
directly — all already `pub(crate)`, all pure IMAP/batching mechanics with
no Markdown coupling. `email-pull` gets its own `gather_pending`/
`run_worker`/`process_batch_on_session`, adapted copies of `email_sync`'s
equivalents, following this codebase's own established precedent of
duplicating until a *second or third* real consumer justifies extraction
(`decrypt_files` duplicated `ConcurrencyInput`/`ConfirmInput` as the
2nd consumer; only `pull_transform`, the 3rd, triggered hoisting them into
`shared_wizard.rs`, ADR-0074 §0). `email_sync` is the only current consumer
of the worker-loop/checkpoint shape, so `email-pull`, as the 2nd consumer,
still duplicates it rather than generalizing `email_sync::worker`/
`CheckpointEntry` in place.

Own `PullCheckpointEntry { mailbox: String, uid: u32, attachments:
Vec<(String, String)> }` (hash, scratch-relpath pairs) — no
`message_hash`/`md_staged_relpath`/`desired_md_name`/`mailbox_tag` fields,
since there's no message-dedup or frontmatter to support.

Per UID: `fetch_uids` writes the `.eml` straight to its final mailbox path;
then it's parsed with `mail_parser::MessageParser` (the same crate
`transform.rs` already uses) and, for each `message.attachments()` part,
the bytes are hashed (MD5) and written to a UID-keyed scratch directory —
no HTML-body conversion, no frontmatter rendering, no `htmd` involvement at
all. A structural verify step (a simplified sibling of
`transform::verify_transformed`) confirms the `.eml` exists non-empty and
every staged attachment scratch file is non-empty before the checkpoint
entry is appended and the queue moves on.

### 6. Dedup + placement phase (sequential, new `email_pull/dedup.rs`)

Mirrors `pull_transform::dedup::place_files`'s shape directly — content-hash
check against `.attachment-hashes` (hit: discard the scratch copy, count a
duplicate; miss: move it to `<identity_dir>/attachments/`, disambiguating a
same-run name collision via `core::data::unique_path`, then commit the hash)
— rather than `email_sync::dedup::run_dedup_pass`'s two-pass message/
attachment split. There's no message pass here at all, only attachments.

### 7. Upload phase (concurrent, optional)

Reuses `commands/job/upload.rs`'s `pending_upload_tasks`/`run_upload_phase`/
`UploadedIndex` unchanged, always called with `encryptor: None` (§4).

## Consequences

- A second, adapted copy of `email_sync`'s IMAP worker-loop/checkpoint
  machinery now exists (`email_pull/{manifest,worker}.rs`) alongside the
  original, rather than one generalized implementation — an explicit,
  precedent-following tradeoff (see §5), not an oversight. A future fix to
  `email_sync::worker`'s retry/reconnect logic (e.g. another ADR-0071/0080-
  style hardening) will need a matching fix applied to `email_pull::worker`
  by hand; this is the same maintenance cost ADR-0074 accepted for
  `ConcurrencyInput`/`ConfirmInput` before their 3rd-consumer extraction.
- `email-pull`'s output tree has no cross-file references at all (no
  frontmatter, no `also-in:`, no attachment-list field) — simpler to reason
  about than `email-sync`'s output, but also means nothing records which
  message(s) a deduped attachment originally came from beyond what's still
  recoverable by inspecting the `.eml` files' own MIME headers.
- Because raw messages are never deduped against each other, a mailbox with
  many forwarded/duplicate copies of the same message produces one `.eml`
  per copy (by design) even though their attachments collapse to one file
  each.

## Out of scope

- Whole-message dedup or any raw-`.eml` merge semantics — attachments only,
  per the explicit ask, and the only way to avoid inventing a new "amend a
  canonical raw file" mechanism that raw `.eml` has no safe equivalent for.
- Encryption of any kind for this job (§4) — no `--encryption-key` flag, no
  bucket-default-key prompt, ever.
- Generalizing `email_sync`'s worker-loop or `CheckpointEntry` into a module
  shared with `email_pull` — explicitly deferred per this codebase's own
  "extract on the third consumer" precedent (ADR-0074 §0), not an oversight.
- Mailbox-name modified-UTF-7 decoding — inherits the same long-standing,
  unsolved gap `sink`/`transform` have always had (ADR-0005/0006); directory
  names stay in their sanitized raw wire form.
