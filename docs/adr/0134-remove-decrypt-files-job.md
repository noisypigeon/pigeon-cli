# ADR-0134: remove `pigeon job run decrypt-files`

- **Author**: Willow Graysen ([@noisypigeon](https://github.com/noisypigeon)).
- **Date**: 2026-10-12.
- **Status**: Accepted.

## Context

`pigeon job run decrypt-files` (ADR-0028) is a purely local, one-way "get my
plaintext back" tool: given a directory of already-downloaded `*.enc` files
and an encryption-key alias, it decrypts every one back to plaintext under an
output directory. It was added as `core::job::Job`'s second implementor, a
deliberate validation of that trait's genericity, but has no evidenced
ongoing use — no other job, ADR, or shared helper has ever depended on it
being present, and `Encryptor::decrypt` (`src/core/crypto.rs`), the one piece
of `core::crypto` it exercises, has never had a caller outside this job and
its own round-trip unit tests since ADR-0025 first speculated about it.

This mirrors the exact redundancy argument ADR-0094 (`sort`) and ADR-0103
(`reduce`) already made for a job type carrying its own CLI surface, module,
and test suite without a distinguishing capability anything else in the
codebase relies on. A full dependency census (below) confirms the same
one-directional shape those two removals had: `decrypt-files` is a consumer
of several shared helpers, never a provider of anything another job type
uses.

## Decision

Delete `src/commands/job/decrypt_files/` wholesale and prune every reference
to it. Un-hoist nothing: every shared helper `decrypt-files` used —
`CpuConcurrencyInput` and `ConfirmInput` (`shared_wizard.rs`), `ReportBucketInput`
plus `report_upload.rs` (ADR-0100), `Store::prompt_select_encryption_key`
(ADR-0026), and `credentials::get_secret` — has at least one other active
consumer today (`email_sync`, `deduplicate`, `transform`, or `keyring` itself)
and needs no relocation. This is a clean deletion, not a refactor, confirmed
by the same kind of census ADR-0094 §Decision performed for `sort`.

### Code

- Delete `src/commands/job/decrypt_files/{mod,wizard,worker}.rs` wholesale.
- `src/commands/job/mod.rs`: remove `pub mod decrypt_files;`.
- `src/commands/job/cli.rs`: remove the `JobType::DecryptFiles { .. }`
  variant and its `Observable::command_name()` arm (`"job.decrypt-files"`).
- `src/commands/job/commands.rs`: remove `decrypt_files` from the job-module
  `use` list and its dispatch match arm.
- `src/core/job.rs`: the `Job` trait's own doc comment named
  `decrypt_files::DecryptFilesJob` as its "first real validation" of
  multi-implementor genericity and cited it as an example of a job with no
  upload phase — both corrected to state `EmailSyncJob` as the trait's sole
  current implementor, with a plain historical note that `DecryptFilesJob`
  existed and was removed here.
- `src/core/crypto.rs`: `Encryptor::decrypt` and its impl are deleted
  outright, not re-annotated `#[allow(dead_code)]` (its pre-ADR-0028 state).
  Nothing has called it since ADR-0025 first added it speculatively "for a
  future download/restore command"; that future arrived once (ADR-0028) and
  is now gone again, so this repo's standing discipline of deleting code
  that serves no real caller (most recently exercised by ADR-0133 §8's
  forced-deletion census) applies rather than preserving it dormant a second
  time. Its three dedicated round-trip/tamper/short-input unit tests are
  deleted with it; `encrypt`'s own tests (the still-live upload path,
  ADR-0025) are untouched.
- `src/observability/resources.rs`, `src/commands/job/email_sync/wizard.rs`,
  `src/commands/job/shared_wizard.rs`: trim stale `decrypt_files`
  cross-references in doc comments (an example list, an "originated by"
  attribution, and a consumer count that drops from three to two for
  `default_concurrency`, now `deduplicate` and `transform` only).
- `tests/cli.rs`: remove the 4 `decrypt_files`-specific integration tests
  (`job_run_help_lists_decrypt_files`,
  `job_run_decrypt_files_help_shows_input_output_and_key_flags`,
  `job_run_decrypt_files_without_input_dir_fails_fast_non_interactively`,
  `job_run_decrypt_files_rejects_same_input_and_output_dir`).

### Documentation

- `docs/adr/0028-decrypt-files-job.md`: `Status` marked `Superseded by
  ADR-0134 — job removed` (same pattern as ADR-0083's `Reversed by
  ADR-0094` marking) — body left untouched as the historical record of why
  the job existed and how it worked.
- `CHANGELOG.md`: one new `[Unreleased]` bullet for this ADR.

## Consequences

- `pigeon` loses its one non-upload-capable job type; every remaining job
  type (`email-sync`, `email-pull`, `deduplicate`, `transform`, `rclone
  copy`/`delete`) is upload-capable.
- A user who genuinely wants to reverse an encrypted upload loses a built-in
  tool for it. `core::crypto::Aes256GcmSivEncryptor`'s decrypt logic is fully
  documented in this ADR's and ADR-0028's history if a future need
  resurfaces it — a small, well-understood addition, not a reason to keep
  the method dormant speculatively a second time.
- `core::job::Job` returns to having exactly one implementor
  (`EmailSyncJob`), the same state it was in before ADR-0028. The trait's own
  genericity claim is no longer backed by a second real implementor in the
  codebase, which its doc comment now states plainly rather than pointing at
  a deleted type.

## Out of scope

- Any change to `email-sync`'s encrypt-before-upload path (ADR-0025/0027) —
  that direction is unaffected; only the reverse (decrypt) tool is removed.
- Re-adding decrypt functionality in a different shape (e.g. folded into
  another job) — not proposed here.

## Verification

- `mise run ci` clean (fmt-check + lint + test).
- `cargo clippy --all-targets --all-features -- -D warnings` clean — the
  mechanical proof that every shared helper `decrypt-files` used genuinely
  has another consumer and needed no un-hoisting.
- `pigeon job run --help` no longer lists `decrypt-files`; `pigeon job run
  decrypt-files --help` fails as an unrecognized subcommand.
- Repo-wide grep for `decrypt_files`/`DecryptFiles`/`decrypt-files` turns up
  nothing left to update outside this ADR's own text and ADR-0028's
  historical record.
