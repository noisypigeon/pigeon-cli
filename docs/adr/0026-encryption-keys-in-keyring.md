# ADR-0026: encryption keys in the pigeon keyring

- **Author**: Willow Finch ([@noisypigeon](https://github.com/noisypigeon)).
- **Date**: 2026-09-24.
- **Status**: Accepted.

## Context

ADR-0025 deliberately kept pigeon out of encryption-key generation and storage: the user generates their own key, stores it in their own 1Password vault, and injects it via `PIGEON_ENCRYPTION_KEY` at runtime -- pigeon never sees or persists it. The user now wants the opposite for this specific secret: manage it the same way as every other secret pigeon already manages (email app passwords, bucket S3 secret keys) via `pigeon keyring add`, backed by the same `keyring.toml`/OS-keychain machinery (ADR-0022), and select which key to use interactively when running a job -- mirroring how `--remote-output` already selects a bucket-config alias.

This is a direct, explicit reversal of ADR-0025 §2's `Aes256GcmSivEncryptor::from_env()` constructor and all of ADR-0025 §3 ("Key sourcing: one environment variable, never persisted by pigeon"), so per this project's convention of flagging rather than silently diverging from a prior decision, it gets its own ADR instead of an undocumented edit to 0025. ADR-0025's underlying crypto design -- AES-256-GCM-SIV, HKDF-derived subkeys, content-derived deterministic nonce, the `Encryptor` trait, `.enc`-suffixed keys, `upload_if_changed`'s dedup -- is entirely unchanged by this ADR; only *where the key comes from* changes. `BucketConfig.encrypt: bool` (ADR-0025 §4) is also unchanged: it still governs whether a given bucket requires a key at all -- this ADR only changes *which* key gets used and *where it's sourced from*.

Three design points were resolved directly with the user before writing this decision:

- Should a generated key ever be displayed for the user's own independent backup, given the OS keychain becomes its only storage? -> No, never displayed. The OS keychain is the sole source of truth, exactly like every bucket/email secret this codebase already stores this way with no independent backup path either.
- Should `PIGEON_ENCRYPTION_KEY` keep working as a fallback alongside keyring-based keys? -> No, fully retired. One key-sourcing path, no precedence rule to maintain.
- Should `keyring add encryption-key` always generate a key itself, or allow pasting one in? -> Prompts for a key, defaulting to a freshly generated one if left blank. This also gives anyone already using ADR-0025's `PIGEON_ENCRYPTION_KEY` workflow a migration path: paste their existing key in under a new alias so already-uploaded ciphertext stays decryptable.

Prior research into this codebase established the exact extension points this decision uses:

- `Entry` (`src/commands/keyring/store.rs`) is `#[serde(tag = "kind", rename_all = "kebab-case")]` over `Email(Identity)`/`Bucket(BucketConfig)` -- a third variant serializes with no further serde work.
- `Store::bucket_configs()`/`prompt_select_bucket()` (empty/single-auto-select/many-`Select` logic, and its own "no bucket-configs configured..." error message) are the exact pattern a third kind's iterator/selector mirrors.
- `core::keyring::credentials::set_secret`/`get_secret`/`delete_secret` (`src/core/keyring/credentials.rs`) are already generic over any alias under one shared `"pigeon"` OS-keychain service -- reusable with zero new plumbing.
- `AddKind`/`wizard::add`'s dispatch (`src/commands/keyring/wizard.rs`) and `wizard::modify`'s `match entry { Entry::Email(..) => .., Entry::Bucket(..) => .. }` both already route generically per kind -- each needs one new arm.
- `RemoteOutputInput` (`src/commands/job/email_sync/wizard.rs`) is the exact `WizardInput` shape to mirror; the `Aes256GcmSivEncryptor::from_env()` fail-fast block it replaces sits right after `job.remote` is resolved, before the concurrency/proceed prompts.
- No date/timestamp crate or pattern exists anywhere in this codebase today -- `created_at` is a genuinely new precedent needing a new dependency.
- `Aes256GcmSivEncryptor::from_hex_key` is already a standalone constructor independent of `from_env` -- it becomes the only way an encryptor gets built once `from_env` is deleted.
- `aes_gcm_siv::aead::OsRng` + `Aes256GcmSiv::generate_key` are already available through the existing `aes-gcm-siv` dependency -- key generation needs no new crate for randomness.

## Decision

### 1. New keyring entry kind: `EncryptionKey`

New file `src/commands/keyring/encryption_key.rs`:

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EncryptionKey {
    pub alias: String,
    /// RFC3339, set once at creation; never updated by `modify` -- rotating
    /// the secret changes the key, not when the entry was made.
    pub created_at: String,
}

impl crate::core::keyring::KeyringEntry for EncryptionKey {
    fn alias(&self) -> &str {
        &self.alias
    }
    fn kind(&self) -> &'static str {
        "encryption-key"
    }
    fn detail(&self) -> String {
        format!("created {}", self.created_at)
    }
}
```

The secret itself (64 hex characters) is stored via the existing `credentials::set_secret`/`get_secret`/`delete_secret`, keyed by alias, exactly like every other secret in this codebase -- no new keychain plumbing.

`Entry` gains a third variant:

```rust
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum Entry {
    Email(Identity),
    Bucket(BucketConfig),
    EncryptionKey(EncryptionKey),
}
```

serializing as `kind = "encryption-key"` automatically. `impl KeyringEntry for Entry` gains a third delegating match arm, identical in shape to the existing two. `Store` gains `encryption_keys()` (mirroring `bucket_configs()`) and `prompt_select_encryption_key()` (mirroring `prompt_select_bucket()`, including its empty/single-auto-select/many-`Select` behavior and its own "no encryption-keys configured; run 'pigeon keyring add encryption-key' first" error message).

### 2. `pigeon keyring add encryption-key [ALIAS]`

New `AddKind::EncryptionKey { alias: Option<String> }` (`src/commands/keyring/cli.rs`) and a new `wizard::add_encryption_key`, following `add_bucket`'s shape:

- Prompts `Alias` if not given on the command line (same `store.contains_alias` global-uniqueness check every other kind already uses).
- Prompts `read_secret("Key (press enter to use a freshly generated one)")`. Empty input generates a key; non-empty input is validated via `Aes256GcmSivEncryptor::from_hex_key` before saving -- this both catches a malformed pasted key immediately and doubles as the import path for an existing ADR-0025-era key.
- New `core::crypto::generate_hex_key() -> String`: 32 random bytes from `Aes256GcmSiv::generate_key(&mut aes_gcm_siv::aead::OsRng)` (already available through the existing `aes-gcm-siv` dependency), hex-encoded.
- Never prints the key, at generation or otherwise -- per the user's explicit choice above, and consistent with how every other secret this wizard collects is handled.
- `created_at = time::OffsetDateTime::now_utc().format(&time::format_description::well_known::Rfc3339)`, stored as a plain `String` field (no `time`-side serde integration needed). New dependency: `time = "0.3"`.
- `credentials::set_secret` then `store.push(Entry::EncryptionKey(...))`/`store.save`, with the same delete-secret-on-save-failure rollback `add_bucket` already uses.

`wizard::add`'s bare (no-subcommand) `Select` gains a third item, `"Encryption key"`, alongside `"Email identity"`/`"Bucket-config"`. `wizard::modify`'s match gains a third arm, `Entry::EncryptionKey(key) => modify_encryption_key(&mut store, &path, &key)`, which only rotates the secret (`created_at` stays immutable, per its own doc comment above) -- same "press enter to keep current" shape `modify_bucket` already uses for its secret.

### 3. `pigeon job run email-sync --encryption-key <alias>`, resolved interactively otherwise

New `encryption_key: Option<String>` flag on `JobType::EmailSync` (`src/commands/job/cli.rs`), threaded through `commands.rs` and `email_sync::wizard::dispatch`/`dispatch_async` exactly like `remote_output` already is.

New `EncryptionKeyInput<'a>` (`src/commands/job/email_sync/wizard.rs`), a `WizardInput` impl mirroring `RemoteOutputInput`:

```rust
struct EncryptionKeyInput<'a> {
    flag: Option<String>,
    store: &'a Store,
    required: bool, // = job.remote.as_ref().is_some_and(|(bc, _)| bc.encrypt)
}
```

`prompt()` returns `Ok(None)` immediately when `!required` -- no bucket targeted, or the targeted bucket has `encrypt: false` -- so the common, unencrypted case gets no new prompt at all. When `required`, it calls `store.prompt_select_encryption_key()`: zero encryption-keys configured surfaces as a clear error, exactly one auto-selects silently, more than one shows a `Select`.

This replaces ADR-0025's `Aes256GcmSivEncryptor::from_env()` fail-fast block in `dispatch_async` (same insertion point: right after `job.remote` is resolved, before the concurrency/proceed prompts) with:

```rust
let resolved_encryption_key_alias = match (EncryptionKeyInput {
    flag: encryption_key,
    store: &keyring_store,
    required: job.remote.as_ref().is_some_and(|(bc, _)| bc.encrypt),
})
.resolve()
{
    Ok(alias) => alias,
    Err(err) => return fail(err),
};
job.encryptor = match resolved_encryption_key_alias {
    Some(alias) => {
        let key_hex = match credentials::get_secret(&alias) {
            Ok(secret) => secret,
            Err(err) => return fail(err),
        };
        match Aes256GcmSivEncryptor::from_hex_key(&key_hex) {
            Ok(encryptor) => Some(encryptor),
            Err(err) => return fail(err),
        }
    }
    None => None,
};
```

Still fails fast in the same place ADR-0025 did -- before concurrency/proceed prompts, before any fetch/transform work -- only the source of the key material changes, from an env var to a keyring alias's OS-keychain secret.

### 4. Retire the env-var path

`Aes256GcmSivEncryptor::from_env()` and `ENCRYPTION_KEY_ENV` are deleted from `src/core/crypto.rs`. `from_hex_key` is untouched and becomes the only way an `Aes256GcmSivEncryptor` gets built, called from both `wizard::add_encryption_key`'s validate-before-save check (§2) and the job wizard's resolution (§3).

## Consequences

- One key-sourcing path, matching every other pigeon secret: `pigeon keyring add/modify/delete/list`, backed by `keyring.toml` + OS keychain (ADR-0022's unified model), instead of a bespoke env-var convention unique to encryption.
- Multiple named encryption keys can coexist (aliased), letting different buckets or runs use different keys -- `BucketConfig.encrypt: bool` still just gates *whether* a key is required; *which* key is chosen per job run via `--encryption-key`/interactive selection, not stored on the bucket-config itself.
- No independent backup of a generated key exists outside the OS keychain (per the user's explicit choice) -- losing that keychain entry (OS reinstall, keychain corruption, migrating machines without exporting it) makes every file encrypted under that key permanently unrecoverable. This is a real, accepted risk, structurally identical to the risk every existing bucket/email secret already carries in this codebase.
- Anyone who adopted ADR-0025's `PIGEON_ENCRYPTION_KEY` workflow before this ADR must run `pigeon keyring add encryption-key` and paste in their existing key (not generate a new one) to keep decrypting previously-uploaded ciphertext -- the "paste an existing key" path (§2) exists specifically to make this possible.
- New dependency: `time = "0.3"`, this codebase's first date/timestamp crate.
- No change to the actual encryption scheme (AES-256-GCM-SIV, deterministic nonce, `Encryptor` trait, `.enc`-suffixed keys, `upload_if_changed` dedup) -- ADR-0025 §1 stands entirely as decided.

## Out of scope

- Key rotation propagating to already-uploaded ciphertext -- rotating `modify encryption-key`'s secret only affects *future* uploads; old objects stay encrypted under whatever key was active when they were uploaded, so decrypting them later still requires that old key under some alias. ([#31](https://github.com/noisypigeon/pigeon-cli/issues/31))
- Any command to export or display a stored encryption key after creation -- deliberately never shown, per the user's choice above.
- Persisting "which encryption-key alias to use" on `BucketConfig` itself -- selection stays a per-job-run choice, not stored on the bucket-config.

Implementation is a separate, later task.
