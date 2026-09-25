# Changelog

All notable changes to this module are documented in this file.

The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

## [Unreleased]

- ADR-0029: Every substantive change now lands via branch → PR → local `mise run ci` gate → auto-merge, with a changelog entry per PR ([#1](https://github.com/noisypigeon/pigeon-cli/pull/1)).

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
