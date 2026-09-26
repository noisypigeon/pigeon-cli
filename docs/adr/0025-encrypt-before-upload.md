# ADR-0025: client-side encryption before bucket upload

- **Author**: Willow Finch ([@noisypigeon](https://github.com/noisypigeon)).
- **Date**: 2026-09-24.
- **Status**: Accepted.

## Context

The user wants files encrypted client-side before `pigeon job run email-sync` uploads them to a bucket, so that anyone who gains unauthorized access to the bucket -- but not the key -- finds its contents useless. Three requirements were given directly:

1. Build it as a trait and a consumer, not a one-off bolted onto the upload path, so future file/data ops can reuse the same encryption.
2. The user generates their own key, following recommendations, and stores it in their own 1Password vault -- pigeon does not generate, store, or integrate with 1Password for this key.
3. Duplicate detection must keep working in the bucket, as long as the same key is used for the files being compared.

Requirement 3 is the crux of the design. Encryption normally randomizes ciphertext via a random nonce, so two uploads of identical plaintext produce different ciphertext and defeat `upload_if_changed`'s existing MD5-vs-S3-ETag comparison (`service/pigeon-cli/src/commands/keyring/bucket/client.rs`). The fix is **deterministic encryption**: derive the nonce from the plaintext itself, keyed by the secret, so `Enc(key, plaintext)` is always byte-identical for the same `(key, plaintext)` pair. This was confirmed directly with the user as an accepted tradeoff: an attacker with bucket access (but not the key) can tell that two ciphertext objects are duplicates of each other, but not what they contain.

The key model was also confirmed directly with the user: a single **symmetric** secret key, not an asymmetric keypair. "Private key" in the original request meant "my own secret," not X25519/age-style public/private separation -- age's standard construction uses ephemeral per-file keys and a random nonce via HKDF, which is fundamentally incompatible with deterministic dedup without abandoning age's own tooling guarantees for no benefit here.

Prior research into this codebase established:

- The single choke point for intercepting bytes before they leave the process is `service/pigeon-cli/src/commands/job/email_sync/worker.rs`'s `upload_one` -- specifically the `fs::read(&task.path)` call and the `client::upload_if_changed(...)` call right after it.
- `upload_if_changed` computes `md5::compute(&data)` and compares it to S3's ETag; it needs **zero logic changes** -- it keeps working correctly as long as what it's handed is deterministic ciphertext instead of plaintext.
- Local content-hash dedup (`core::data::ContentIndex`, `commands::job::email_sync::dedup::EmailDedup`, per ADR-0012/0020/0021) operates entirely on **plaintext** bytes, before and independent of upload -- this ADR does not touch it.
- Trait conventions (ADR-0023): traits live in `service/pigeon-cli/src/core/`, are `pub(crate)`, and it's an accepted, explicitly-named tradeoff to introduce a trait with exactly one real implementor for structural consistency (precedent: `Job`, `Transform`, `Dedup`).
- No existing precedent for sourcing a secret from outside pigeon's own `keyring.toml`/OS-keychain `Store` (`service/pigeon-cli/src/commands/keyring/store.rs`, `service/pigeon-cli/src/core/keyring/credentials.rs`) -- every current secret is either typed interactively (`core::wizard::read_secret`) or stored via `keyring::Entry` under service `"pigeon"`. This ADR establishes a new, narrow precedent: one environment variable, read once, never persisted by pigeon.
- `BucketConfig` (`service/pigeon-cli/src/commands/keyring/bucket/store.rs`) is a small `Serialize`/`Deserialize` struct (`alias`, `endpoint`, `bucket`, `access_key_id`) -- a new `#[serde(default)] encrypt: bool` field is additive and won't break existing `keyring.toml` entries.

## Decision

### 1. Algorithm: AES-256-GCM-SIV (RFC 8452) with a content-derived nonce

Rejected alternatives:

- **Plain AES-256-GCM or ChaCha20-Poly1305 with a random nonce** -- the standard choice for encrypting arbitrary data, but a random nonce defeats requirement 3 outright: identical plaintext would never produce identical ciphertext.
- **age (X25519 keypair, ChaCha20-Poly1305)** -- age's construction uses an ephemeral per-file key and a random nonce derived via HKDF; forcing it deterministic means abandoning age's own tooling and interoperability guarantees for no benefit here, and conflicts with the user-confirmed symmetric-key decision above.
- **AES-SIV (RFC 5297)** -- a genuinely deterministic AEAD construction, and the "purist" choice for this exact problem. Not chosen: slower two-pass CMAC-based construction, and much thinner Rust ecosystem support than GCM-SIV.

Chosen: **AES-256-GCM-SIV**. It is nonce-misuse-resistant by construction (RFC 8452's whole point) -- even if the deterministic-nonce derivation below ever collided across two *different* plaintexts under the same key, GCM-SIV degrades to revealing only equality, never the catastrophic plaintext-recovery break that reused-nonce plain GCM would produce. It's single-pass, well-supported via the RustCrypto `aes-gcm-siv` crate, and fits this codebase's existing shape: every file is already fully read into a `Vec<u8>` before `upload_if_changed` is called, so there's no need for streaming/chunked AEAD.

Nonce derivation: HKDF-SHA256 splits the one user-supplied 256-bit key into two domain-separated subkeys -- `enc_key` for AES-256-GCM-SIV, `nonce_key` for nonce derivation -- so the same secret is never used directly as both a cipher key and a hash key. The nonce is the first 12 bytes of `HMAC-SHA256(nonce_key, plaintext)`. Because this is a pure function of `(key, plaintext)`, identical plaintext always yields an identical nonce, hence identical ciphertext, under the same key -- this is what makes requirement 3 hold with zero changes to `upload_if_changed`.

Wire format: `nonce (12 bytes) || AES-256-GCM-SIV ciphertext+tag`. No AAD is bound in -- deliberately, so identical content at two different logical paths still produces identical ciphertext, matching this codebase's existing byte-identical-content dedup philosophy (ADR-0012).

New dependencies: `aes-gcm-siv`, `hkdf`, `sha2`, `hmac`, `hex` -- all small, widely-used RustCrypto-family crates, the same quality bar as this project's existing `md5` dependency.

### 2. New trait `Encryptor` in `service/pigeon-cli/src/core/crypto.rs`, one real implementor alongside it

```rust
/// Behavior shared by every place this CLI needs to turn plaintext bytes
/// into bytes safe to store somewhere an attacker might read without the
/// key, and back again. One real implementor today (`Aes256GcmSivEncryptor`,
/// this module) backs `pigeon job run email-sync`'s upload phase (ADR-0025);
/// the trait exists so a future file/data op can reuse the same
/// deterministic, dedup-safe construction without depending on the upload
/// phase itself.
pub(crate) trait Encryptor {
    fn encrypt(&self, plaintext: &[u8]) -> Result<Vec<u8>, String>;
    fn decrypt(&self, ciphertext: &[u8]) -> Result<Vec<u8>, String>;
}
```

Unlike `Transform`/`Dedup`/`KeyringEntry` -- trait in `core`, implementor living with its concrete *kind* in `commands` -- `Aes256GcmSivEncryptor` is kind-agnostic by design: there's no "email flavor" vs. "bucket flavor" of encryption. Putting the concrete struct under `commands::keyring::bucket` would misleadingly scope it to buckets when the explicit intent is reuse across future file/data ops. It lives in `core::crypto` alongside the trait, the same reasoning `core::wizard` already uses to host `read_secret`/`confirm` as shared code rather than splitting per-kind.

```rust
pub(crate) struct Aes256GcmSivEncryptor {
    cipher: aes_gcm_siv::Aes256GcmSiv,
    nonce_key: [u8; 32],
}

impl Aes256GcmSivEncryptor {
    /// `key_hex` is 64 lowercase hex characters (32 bytes / 256 bits),
    /// sourced from `PIGEON_ENCRYPTION_KEY` -- never generated or stored by
    /// pigeon itself (see Decision §3). HKDF-SHA256 splits it into two
    /// domain-separated subkeys so the user-supplied secret is never used
    /// directly as both a cipher key and a hash key.
    pub(crate) fn from_hex_key(key_hex: &str) -> Result<Self, String> {
        // hex-decode to 32 bytes, HKDF-SHA256-expand into enc_key/nonce_key
        // with distinct "pigeon-file-enc"/"pigeon-file-nonce" info strings
    }
}

impl Encryptor for Aes256GcmSivEncryptor {
    fn encrypt(&self, plaintext: &[u8]) -> Result<Vec<u8>, String> {
        // nonce = HMAC-SHA256(nonce_key, plaintext)[..12]
        // ciphertext = cipher.encrypt(nonce, plaintext)
        // Ok(nonce || ciphertext)
    }

    fn decrypt(&self, ciphertext: &[u8]) -> Result<Vec<u8>, String> {
        // split first 12 bytes as nonce, cipher.decrypt(nonce, rest)
    }
}
```

`decrypt` exists now for symmetry, round-trip unit testing, and so a future download/restore command has it ready -- but this ADR does not add a CLI command that calls it (see Out of scope).

### 3. Key sourcing: one environment variable, never persisted by pigeon

- The user generates the key themselves, following the recommendation `openssl rand -hex 32` -- a CSPRNG-sourced 256-bit key, matching AES-256's key size. Pigeon does not generate or suggest generating it via its own code.
- The user stores the resulting 64-hex-char string as a 1Password item and injects it at invocation time via 1Password's own tooling (e.g. `op run -- pigeon job run email-sync ...`, or equivalent shell integration). Pigeon adds no 1Password SDK/CLI dependency and never talks to 1Password directly.
- Read from `PIGEON_ENCRYPTION_KEY` at process start. Deliberately not stored via `core::keyring`/`Store`/`keyring.toml`, the existing per-alias secret mechanism -- the user was explicit that 1Password, not pigeon's own keychain entry, is the single source of truth for this secret.

### 4. Per-bucket opt-in: `encrypt: bool` on `BucketConfig`

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BucketConfig {
    pub alias: String,
    pub endpoint: String,
    pub bucket: String,
    pub access_key_id: String,
    #[serde(default)]
    pub encrypt: bool,
}
```

`#[serde(default)]` keeps existing `keyring.toml` entries valid, defaulting to `encrypt: false`. The `bucket-config new`/`edit` wizard flows (`service/pigeon-cli/src/commands/keyring/wizard.rs`) gain one confirm prompt: "Encrypt files before upload to this bucket?".

### 5. Integration point: `worker.rs::upload_one`, at the existing `fs::read`/`upload_if_changed` boundary

Before any fetch/transform work starts, `run_email_sync_job` checks whether any selected identity's target `bucket_config.encrypt` is `true`. If so, it reads `PIGEON_ENCRYPTION_KEY` and constructs one `Aes256GcmSivEncryptor` up front, failing fast with a clear error if the env var is missing or malformed -- not silently, and not only discovered hours into a long run at the upload phase.

In `upload_one`, immediately after `fs::read(&task.path)` and before `client::upload_if_changed(...)`: if encryption is active for this task's bucket, `data = encryptor.encrypt(&data)?`, and the S3 key gets a `.enc` suffix appended (e.g. `mailbox/uid.eml` -> `mailbox/uid.eml.enc`), so a human browsing the bucket -- or future tooling -- can immediately tell ciphertext from plaintext objects.

No changes are needed inside `client::upload_if_changed` itself -- it already just MD5-hashes whatever `data` it's given and compares that to the S3 ETag; deterministic ciphertext in, correct dedup out.

## Consequences

- Files at rest in the bucket are unreadable without `PIGEON_ENCRYPTION_KEY` -- satisfies the stated threat model directly.
- Files at rest on local disk (staging/output dirs) remain plaintext, unchanged from today -- this ADR is scoped to the bucket-upload boundary only, not local-disk-at-rest.
- Upload-time duplicate detection (`upload_if_changed`'s ETag comparison) keeps working exactly as before, with zero logic changes to `client.rs`, as long as the same key is used across the files being compared -- directly satisfies requirement 3.
- Local content-hash dedup (`ContentIndex`/`EmailDedup`) is entirely untouched -- it runs on plaintext before this ADR's encryption step ever applies.
- `Encryptor`/`Aes256GcmSivEncryptor` are generically reusable by any future file/data op that needs the same deterministic, dedup-safe construction -- no dependency on the email-sync job or bucket upload specifically.
- Five new dependencies (`aes-gcm-siv`, `hkdf`, `sha2`, `hmac`, `hex`), all small, widely-used RustCrypto crates.
- **Gap, called out explicitly**: flipping `encrypt: true` on for a bucket that already has plaintext objects at those keys does not retroactively encrypt what's already there. `upload_if_changed` treats the new, `.enc`-suffixed key as new content and uploads it, leaving the old plaintext object sitting alongside it untouched. A one-time migration sweep for pre-existing plaintext objects is a separate, later concern.

## Out of scope

- Any CLI command that calls `Encryptor::decrypt` -- no `pigeon ... download`/`decrypt` command yet. The trait supports it for symmetry and future reuse, but surfacing it is a separate ADR once download/restore tooling exists.
- Any 1Password SDK/CLI integration inside pigeon itself.
- Per-bucket distinct keys -- one global key, via one env var, covers every bucket with `encrypt: true`.
- Migrating or re-encrypting objects already uploaded as plaintext before `encrypt` was turned on for a given bucket. ([#31](https://github.com/noisypigeon/pigeon/issues/31))

Implementation is a separate, later task.
