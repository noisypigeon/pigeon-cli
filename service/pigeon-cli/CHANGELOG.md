# Changelog

All notable changes to this module are documented in this file.

The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

## [Unreleased]

- ADR-0029: Every substantive change now lands via branch → PR → local `mise run ci` gate → auto-merge, with a changelog entry per PR ([#1](https://github.com/noisypigeon/pigeon-cli/pull/1)).
- ADR-0030: Root-causes silent, total attachment-upload loss to a wrong path reconstruction in `run_dedup_pass` (documents investigation and fix; fix not yet implemented) ([#2](https://github.com/noisypigeon/pigeon-cli/pull/2)).
- ADR-0030: Implements the attachment-placement fix -- `email-sync` attachments are now correctly found, deduped, and uploaded instead of silently lost ([#3](https://github.com/noisypigeon/pigeon-cli/pull/3)).
- ADR-0030: Amends the investigation -- real post-fix runs still lost attachments due to a second, deeper bug (attachment placement only runs for messages canonicalized in the same call), plus a separate malformed-source-message finding (documents both; fix not yet implemented) ([#4](https://github.com/noisypigeon/pigeon-cli/pull/4)).
- ADR-0030: Implements both amendment fixes -- attachment placement now resolves each message's canonical path across runs (a re-run genuinely recovers previously-orphaned attachments), and a malformed source message's phantom zero-byte attachment part no longer rejects the whole message ([#5](https://github.com/noisypigeon/pigeon-cli/pull/5)).
- ADR-0031: Adds `mise run adr-issue` and category labels to file/link GitHub issues for genuinely-deferred ADR Out of scope items, and backfills 33 issues across ADR-0003–0030's still-open items ([#39](https://github.com/noisypigeon/pigeon-cli/pull/39)).
- ADR-0032: Documents adding a progress bar to `job run email-sync`'s previously-silent manifest-gathering phase, and an `ATTACHMENTS` column (an IMAP `BODYSTRUCTURE`-derived estimate) to the wizard's pre-run summary table (documents the decision; implementation not yet done) ([#43](https://github.com/noisypigeon/pigeon-cli/pull/43)).
- ADR-0032: Implements the decision -- the manifest-gathering phase now shows a per-identity connect line and mailbox-scoped progress bar, and the summary table gains its `ATTACHMENTS` column ([#44](https://github.com/noisypigeon/pigeon-cli/pull/44)).
- ADR-0033: Documents closing seven backlog issues raised while investigating an attachments-still-not-working report (both ADR-0030 fixes confirmed correctly present): a dedup-phase progress bar with suspend-safe warnings, structured per-category failure reporting, a narrower manifest attachment estimate shown next to the real per-run count, bounded progress-bar prefix width, and control-character escaping in frontmatter YAML values (documents the decision; implementation not yet done) ([#47](https://github.com/noisypigeon/pigeon-cli/pull/47)).
- ADR-0033: Implements the decision -- the dedup pass now shows a progress bar with a suspend-safe warning, `email-sync`'s failure count breaks down by cause (connect/examine/batch-error/verification/parse-skipped), the manifest attachment estimate also counts a `Content-Type` `name` param and is shown next to the real per-run staged count, every progress-bar prefix is bounded to 24 characters, and `yaml_quote` escapes embedded control characters ([#47](https://github.com/noisypigeon/pigeon-cli/pull/47)).
- ADR-0034: Root-causes the manifest `ATTACHMENTS` estimate being stuck at zero for every identity to `gather_pending`'s persisted-manifest reuse fast-path, which caches a UID's attachment count indefinitely and never invalidates it; decides to drop the reuse fast-path and always re-pull `BODYSTRUCTURE` fresh (documents the decision; implementation not yet done) ([#48](https://github.com/noisypigeon/pigeon-cli/pull/48)).
- ADR-0034: Implements the decision -- `gather_pending` always re-pulls a fresh size/attachment-count manifest for pending UIDs instead of reusing a persisted `.manifest` entry, so the `ATTACHMENTS` estimate can no longer get stuck stale; `.manifest` is now a write-only snapshot, and the now-unused `load_manifest` is removed ([#51](https://github.com/noisypigeon/pigeon-cli/pull/51)).
- ADR-0035: Root-causes `job run email-sync` prompting for the macOS keychain password 5-10 times per run to one distinct keychain item per identity/bucket-config/encryption-key alias, each needing its own OS-level access grant; decides that, on macOS, writing a secret should pre-authorize the running binary via `security add-generic-password -T` so future reads never prompt (documents the decision; implementation not yet done) ([#52](https://github.com/noisypigeon/pigeon-cli/pull/52)).
- ADR-0035: Amended and **rejected** -- a live implementation attempt showed the `-T` grant doesn't suppress repeat Keychain prompts, since `/usr/bin/security` (not `pigeon`) is the process actually requesting access once reads are shelled out; no code landed, problem remains open for a future attempt ([#53](https://github.com/noisypigeon/pigeon-cli/pull/53)).
- ADR-0036: Documents restructuring the repo for a future monorepo -- `src/`/`tests/`/`scripts/` move to `service/pigeon-cli/src/`, `service/pigeon-cli/tests/`, and `.github/scripts/`; `Cargo.toml` stays at repo root with updated `[lib]`/`[[bin]]`/`[[test]]` paths; every existing ADR's historical path citations get rewritten to match (documents the decision; implementation not yet done) ([#54](https://github.com/noisypigeon/pigeon-cli/pull/54)).
- ADR-0036: Amended to also relocate `CHANGELOG.md` (this file) to `service/pigeon-cli/CHANGELOG.md`, and implements the restructure -- `src/`, `tests/`, `scripts/`, and `CHANGELOG.md` move via `git mv`; `Cargo.toml` gains explicit `[lib]`/`[[bin]]`/`[[test]]` paths; `.mise.toml` and `CLAUDE.md`'s Dev cycle instructions updated to match. The historical-citation rewrite across existing ADRs is a separate follow-up PR ([#55](https://github.com/noisypigeon/pigeon-cli/pull/55)).

## [0.2.0] - 2026-09-25

Unstable. Introduces a trait-based core architecture, a unified keyring, and end-to-end client-side encryption for `email-sync` uploads.

- ADR-0028: `pigeon job run decrypt-files` decrypts `*.enc` files from a local input directory into an output directory using a configured encryption key — `core::job::Job`'s second real implementor, alongside `email-sync`.
- ADR-0027: An encryption key can default to a bucket-config (chosen by alias in `keyring add/modify bucket`) and still be overridden per run via `--encryption-key` or interactively; non-interactive runs now pick up that default automatically instead of always skipping encryption.
- ADR-0026: `pigeon keyring add/modify encryption-key` manages symmetric encryption keys — generated or imported — in the OS keychain, selectable by alias wherever a key is needed.
- ADR-0025: `email-sync` uploads can be encrypted client-side with AES-256-GCM-SIV and a content-derived deterministic nonce, so identical plaintext still dedups correctly against S3 ETags even when encrypted.
- ADR-0024: The upload phase now runs concurrently across every selected identity (previously sequential), with its own progress bar and per-file retry-with-backoff.
- ADR-0023: Core logic reorganized into trait-based `src/core/` (`Job`, `Transform`, `Dedup`, `KeyringEntry`, `WizardInput`) with concrete implementations moved to `src/commands/`.
- ADR-0022: `pigeon email authenticate` and `pigeon dataops bucket-config` are replaced by one `pigeon keyring add/modify/delete/list`, backed by a single `keyring.toml` and OS-keychain service.
- ADR-0021: `pigeon job run email-sync` replaces `pigeon email sync` with a wizard that resolves identities/concurrency/upload target, batches work across every identity's mailboxes, and shows a pre-run manifest summary.
- ADR-0020: Dedup and transform primitives (`ContentIndex`, filename sanitization, etc.) genericized for reuse beyond email.
- ADR-0019: Sync now runs fetch → transform+dedup → upload as ordered phases with an `.uploaded` checkpoint, instead of uploading inline per message.

## [0.1.0] - 2026-09-23

Initial release; unstable. Scaffolds `pigeon email sync` end to end: IMAP connectivity, Markdown transform, S3-compatible remote storage, and deduplication.

- ADR-0018: Published to crates.io as `pigeon-cli`.
- ADR-0017: `pigeon remote` renamed to `pigeon dataops`.
- ADR-0016: Fixed a macOS keychain ACL bug that lost stored credentials across rebuilds; CLI cleanup (`--local-output`, `--remote-output`, `list`).
- ADR-0015: Progress bars made safe under concurrent mailbox processing.
- ADR-0014: Mailboxes fetched concurrently, one IMAP session per worker.
- ADR-0013: Per-mailbox progress reporting; sender-controlled attachment names sanitized against path-separator crashes.
- ADR-0012: Byte-identical messages and attachments deduplicated on output (content-hashed, merged via `also-in:` frontmatter).
- ADR-0011: `email sync` can upload its output to a configured remote, with MD5/ETag-based skip-if-unchanged.
- ADR-0010: Remote configuration simplified (`alias`, reordered prompts, a `bucket_exists` check replacing an unreliable list-buckets probe).
- ADR-0009: `pigeon remote` adds rclone-style S3-compatible storage (`configure`/`list-buckets`/`ls`/`lsd`/`copy`).
- ADR-0008: Email-specific code grouped under `src/email/` ahead of a second command group.
- ADR-0007: `sink` and `transform` merged into one `pigeon email sync` command, with `--debug` to isolate either phase.
- ADR-0006: `pigeon email transform` converts `.eml` to flat, frontmattered Markdown via `mail-parser`/`htmd`.
- ADR-0005: `pigeon email sink` downloads mail read-only via IMAP with UID-based resume.
- ADR-0004: `mise run pigeon --` runs the built binary.
- ADR-0003: IMAP auth via per-provider app/bridge passwords (not OAuth2), credentials stored in the OS keychain.
- ADR-0002: CLI scaffolded (clap, Mise-pinned toolchain, stub commands).
- ADR-0001: Initial design for authenticating, sinking, and transforming mail from Gmail, Fastmail, iCloud, and Proton.
