# ADR-0135: plaintext secrets in `keyring.toml`; retire the OS keychain

- **Author**: Willow Graysen ([@noisypigeon](https://github.com/noisypigeon)).
- **Date**: 2026-10-12.
- **Status**: Accepted.

## Context

Every secret this CLI manages (an IMAP app/bridge password, an S3 secret
access key, a symmetric encryption key) has lived in the OS-native secure
credential store since ADR-0022 unified them under one `keyring` crate
service name, with `keyring.toml` holding only non-secret metadata. This has
never worked cleanly:

- **ADR-0035 (Rejected)** investigated and then disproved a fix for macOS
  prompting the Keychain password 5-10 times per `job run email-sync`
  invocation — one distinct access-control grant per keychain item, with no
  way found to pre-trust the running binary for all of them at once. The
  problem was left explicitly unsolved, with "revisit full consolidation
  into fewer keychain items" named as the most promising remaining
  direction.
- **ADR-0085** had to add a Linux-specific kernel-keyutils backend override
  because the default D-Bus Secret Service backend fails outright
  ("No default store has been set") on any headless Linux host — accepting,
  as a named tradeoff, that those secrets don't survive a reboot.
- **ADR-0087** already added a `PIGEON_SECRET_<ALIAS>` env var read-side
  override specifically because a Docker container restart is a harder reset
  than even ADR-0085's in-memory kernel keyring survives — in practice, the
  real deployment process (`noisypigeon/pigeon.dev`'s terraform cloud-init
  template) never writes to the OS keychain at all today; it injects every
  secret via this env var and writes a secret-free `keyring.toml` directly.

The OS keychain's one claimed benefit — secrets protected by OS-level access
control rather than file permissions — is not actually being realized by the
one production deployment that exists, and has cost real, unsolved
engineering effort twice (ADR-0035, ADR-0085) trying to make it workable at
all. This ADR removes it entirely: every secret moves into `keyring.toml`
itself, as plaintext, protected only by file permissions. There is nowhere
safe to keep an encryption-at-rest key for this file that would not simply
recreate the same problem one level down, so no encryption-at-rest is
attempted.

## Decision

### 1. Every keyring entry kind gains its secret as a plain struct field

- `Identity` (`src/commands/keyring/email/identity.rs`) gains `pub secret: String`.
- `BucketConfig` (`src/commands/keyring/bucket/store.rs`) gains `pub secret_key: String`.
- `EncryptionKey` (`src/commands/keyring/encryption_key.rs`) gains `pub key_hex: String`.

None of these three fields gets `#[serde(default)]` — unlike
`max_imap_connections` (ADR-0080), which defaults cleanly to `None` for a
pre-existing entry. A `keyring.toml` written before this ADR has no secret
field at all, and `Store::load` now fails to parse it with a clear TOML
"missing field" error rather than silently loading an empty-string secret.
A security-relevant field silently defaulting to empty is the one failure
mode this ADR must not introduce.

### 2. `src/core/keyring/credentials.rs` collapses to one function

`set_secret`/`get_secret`/`delete_secret` and every `keyring::Entry::new(...)`
call are deleted. In their place:

```rust
pub(crate) fn resolve_secret(alias: &str, stored: &str) -> String {
    std::env::var(env_var_name(alias)).unwrap_or_else(|_| stored.to_string())
}
```

`env_var_name` (ADR-0087) is unchanged. Every call site that read
`credentials::get_secret(&alias)?` now reads
`credentials::resolve_secret(&alias, &entry.secret_field)` — infallible, no
`?`, since there is no longer an OS-level lookup that can fail. This touches
`report_upload::resolve`, `deduplicate/wizard.rs::resolve_bucket`, and the
identity/bucket/encryption-key resolution call sites in
`email_sync::wizard` and `email_pull::wizard`.

### 3. `keyring add`/`modify`/`delete` collapse to plain struct mutation

`keyring/wizard.rs`'s `add_email`/`add_bucket`/`add_encryption_key` no longer
call `credentials::set_secret` — the secret is simply a field on the
`Identity`/`BucketConfig`/`EncryptionKey` literal passed to `store.push(...)`,
persisted by the one `store.save(&path)` call that already exists. The
"roll back the keychain secret if the metadata write fails" dance in all
three `add_*` functions, and in `modify_bucket`/`modify_email`/
`modify_encryption_key`'s secret-rotation branches, is deleted outright —
there is nothing to roll back once the secret and the metadata are the same
write. `modify_*`'s "press enter to keep current" path now reads the
current struct field directly instead of calling `get_secret`; `delete`
no longer calls `delete_secret` separately, since `store.remove` + `save`
already removes the secret with the rest of the entry.

### 4. `Store::save` sets `0600` permissions on Unix

After the existing `std::fs::write`, Unix builds additionally call
`std::fs::set_permissions` via `std::os::unix::fs::PermissionsExt` (already
in `std`, no new dependency) to set `0600` — `keyring.toml`'s only
protection now. Applied unconditionally on every save.

### 5. The `keyring` crate dependency is removed entirely

`Cargo.toml` loses `keyring = "4.2.0"` and the
`[target.'cfg(target_os = "linux")'.dependencies]` override ADR-0085 added.
This makes ADR-0085 moot (nothing left to select a backend for) and makes
rejected ADR-0035's unsolved problem permanently moot too (there is no
keychain prompt left to pre-trust). Neither ADR's text is rewritten; each
gets a short forward-pointing "Amendment" section, per this repo's
established convention for amending a merged ADR without silently rewriting
it (ADR-0101's three trailing amendments are the precedent).

### 6. `PIGEON_SECRET_<ALIAS>` (ADR-0087) is unchanged

It remains the one storage-backend-independent override, now checked
against a plaintext-TOML-backed value instead of a keychain-backed one —
orthogonal to where the secret otherwise lives, so this ADR doesn't touch
its shape.

### 7. No automated migration

There is no code path that reads an old OS-keychain secret and writes it
into the new plaintext field. Every existing `keyring.toml` entry — email
identity, bucket-config, encryption key — must be re-added via `pigeon
keyring add` (re-entering the secret by hand) after upgrading, the same
"one more manual re-add, then it stops recurring" precedent ADR-0016 and
ADR-0085 each already established for this codebase.

## Consequences

- `keyring.toml` becomes the single source of truth for every secret this
  CLI manages. Reading it (or a backup/copy of it) is now equivalent to
  reading every configured IMAP password, S3 secret key, and encryption
  key — the same trust model this codebase already applies to an `rclone.conf`
  or SSH config file, stated explicitly rather than left implicit.
- **No Windows-specific protection is added.** `0600` is Unix-only
  (`#[cfg(unix)]`); a Windows user gets no OS-level protection on
  `keyring.toml` at all once the keychain is gone. Flagged here as a real,
  accepted gap rather than silently left unaddressed — there is no evidenced
  Windows production deployment today, mirroring the "no evidenced Linux
  desktop deployment" reasoning ADR-0085 already used to scope its own fix
  narrowly.
- Keyring-related tests no longer touch a real OS keychain at all (previously
  `keyring_delete_confirmed_deletes_it` exercised a real keychain delete
  call) — a concrete CI-stability win, not just a refactor.
- Rejected ADR-0035's unsolved problem (repeated macOS Keychain prompts) and
  ADR-0085's Linux-specific workaround (secrets not surviving a reboot) are
  both obsoleted at the root rather than patched further.
- A user upgrading across this ADR loses every previously-configured secret
  from pigeon's point of view (the metadata entries remain, but fail to
  load) and must re-run `pigeon keyring add` for each one.

## Out of scope

- Any form of encryption-at-rest for `keyring.toml` itself — there is
  nowhere safe to keep a master key for it that wouldn't recreate the same
  problem one level down.
- An automated migration path from the OS keychain to the new plaintext
  fields.
- Windows-specific file-permission hardening (e.g. an NTFS ACL equivalent)
  — named as a gap, not solved here.
- Any change to `PIGEON_SECRET_<ALIAS>`'s shape or precedence (ADR-0087).

## Verification

- `mise run ci` clean (fmt-check + lint + test).
- `cargo clippy --all-targets --all-features -- -D warnings` clean with the
  `keyring` crate removed from `Cargo.toml` entirely.
- New unit tests: `resolve_secret` prefers the env var over a stored value
  and falls back to the stored value when unset; `Store::load` rejects a
  pre-migration file missing a secret field with a clear error rather than
  loading an empty one; (`#[cfg(unix)]`) `Store::save` sets `0600`
  permissions on the written file; the existing mixed-entry round-trip test
  now also asserts each kind's secret field round-trips through TOML.
- `pigeon keyring add/modify/delete/list` end-to-end against a throwaway
  `PIGEON_CONFIG_DIR`, confirming no test or real invocation touches the OS
  keychain anymore.

## Amendment pointers

Short forward-pointing "Amendment" sections are added to ADR-0022, ADR-0026,
and ADR-0085, each pointing at this ADR. ADR-0035 is already `Rejected` and
historical; its investigation record stays accurate as-is and needs no
amendment.

## Amendment (2026-10-13): `keyring.toml` is relocatable per invocation (ADR-0145)

ADR-0145 adds a `--keyring-path` global flag, so the file this ADR made the
single source of truth for every secret can be named per invocation rather
than only via `PIGEON_CONFIG_DIR`. Nothing about the file's contents,
`0600` permissions, or `PIGEON_SECRET_<ALIAS>`'s precedence changes. Worth
noting alongside this ADR's trust model: the generated `rclone.conf`
(ADR-0136) is written beside the keyring and carries the same plaintext S3
credentials, so relocating one relocates both.
