# ADR-0027: encryption key defaults to the bucket-config, overridable per run

- **Author**: Willow Finch ([@noisypigeon](https://github.com/noisypigeon)).
- **Date**: 2026-09-24.
- **Status**: Accepted.

## Context

ADR-0025 tied encryption to a `BucketConfig.encrypt: bool` flag, set once when the bucket is configured (`pigeon keyring add/modify bucket`'s "Encrypt files before upload to this bucket?" prompt). ADR-0026 built key selection on top of that flag: `EncryptionKeyInput.required` in `src/commands/job/email_sync/wizard.rs` was exactly `job.remote.as_ref().is_some_and(|(bc, _)| bc.encrypt)` -- so the job wizard only ever offered to select an encryption key if the target bucket had been pre-configured that way. There was no interactive path to opt into encryption at run-time for a bucket that wasn't configured with `encrypt: true` up front.

This ADR originally (as first accepted) removed that coupling entirely: encryption became a pure per-job-run interactive choice, mirroring the existing "Upload to a bucket-config?" pattern (`RemoteOutputInput`) -- the job wizard asked "Encrypt this upload?" whenever there was an upload target at all, independent of anything stored on the bucket-config, with `--encryption-key`/interactive selection as the only way to name a key.

That design left a real gap: **non-interactive/scripted runs always skipped encryption silently**, since there was no bucket-level signal to act on and no prompt is ever shown outside a TTY. A bucket that should "always" be encrypted (e.g. a cron-driven backup target) had no way to get that behavior without repeating `--encryption-key <alias>` on every single invocation. This amendment reintroduces bucket-level state -- but as a *default*, not a hard requirement: `BucketConfig` can name an encryption key it encrypts with by default, used automatically (including non-interactively), while `--encryption-key` or declining interactively still overrides it per run. Confirmed directly with the user: tie the key to the bucket-config, but keep the ability to override.

This still supersedes ADR-0025 §4 ("Per-bucket opt-in: `encrypt: bool` on `BucketConfig`") and the `required`-gating half of ADR-0026 §3 (`EncryptionKeyInput`) -- it does not reintroduce either of those; the bucket now stores *which key*, not *whether to encrypt as a requirement*, and the job wizard still always has the final say per run.

## Decision

### 1. `BucketConfig` carries an optional default encryption key

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BucketConfig {
    pub alias: String,
    pub endpoint: String,
    pub bucket: String,
    pub access_key_id: String,
    /// Alias of the encryption key this bucket encrypts with by default
    /// (ADR-0027). `None` means no default -- every job run gets asked
    /// explicitly with no bucket-level nudge either way.
    #[serde(default)]
    pub encryption_key_alias: Option<String>,
}
```

`#[serde(default)]` keeps existing `keyring.toml` entries (which never had this field, or had the old `encrypt: bool`, silently ignored by serde since there's no `deny_unknown_fields`) valid, defaulting to `None`.

### 2. `keyring add/modify bucket` gain a key-selection prompt, not a bool

```rust
let encryption_key_alias = match confirm("Encrypt uploads to this bucket by default?", <seed>) {
    Ok(true) => match store.prompt_select_encryption_key() {
        Ok(key) => Some(key.alias.clone()),
        Err(message) => {
            println!("{message}");
            None
        }
    },
    Ok(false) => None,
    Err(err) => return fail(err),
};
```

`<seed>` is `false` in `add_bucket`, `current.encryption_key_alias.is_some()` in `modify_bucket` (re-selecting the key each time it's confirmed, matching how every other field in `modify_bucket` is re-prompted rather than selectively skipped). A "no encryption-keys configured" result from `prompt_select_encryption_key()` degrades gracefully to `None` (printed, not fatal), matching `RemoteOutputInput`'s existing pattern for an equivalent situation.

`BucketConfig::detail()` surfaces the default in `pigeon keyring list`:

```rust
fn detail(&self) -> String {
    match &self.encryption_key_alias {
        Some(alias) => format!("{} ({}), encrypts with '{alias}'", self.endpoint, self.bucket),
        None => format!("{} ({})", self.endpoint, self.bucket),
    }
}
```

### 3. `EncryptionKeyInput` reads the bucket's default, but the job wizard still decides -- and can always override

```rust
struct EncryptionKeyInput<'a> {
    flag: Option<String>,
    store: &'a Store,
    uploading: bool,             // = job.remote.is_some() -- no upload, nothing to encrypt
    bucket_default: Option<String>, // = job.remote's bucket-config's encryption_key_alias, if any
}

impl WizardInput for EncryptionKeyInput<'_> {
    type Value = Option<String>;

    fn flag_value(&self) -> Option<Result<Option<String>, String>> {
        self.flag.clone().map(|alias| Ok(Some(alias)))
    }

    fn prompt(&self) -> Result<Option<String>, String> {
        if !self.uploading {
            return Ok(None);
        }
        if let Some(default_alias) = &self.bucket_default {
            let use_default = Confirm::with_theme(&ColorfulTheme::default())
                .with_prompt(format!("Encrypt this upload using '{default_alias}'?"))
                .default(true)
                .interact()
                .map_err(|err| format!("failed to read confirmation: {err}"))?;
            if use_default {
                return Ok(Some(default_alias.clone()));
            }
            let use_different = Confirm::with_theme(&ColorfulTheme::default())
                .with_prompt("Use a different encryption key instead?")
                .default(false)
                .interact()
                .map_err(|err| format!("failed to read confirmation: {err}"))?;
            if !use_different {
                return Ok(None);
            }
        } else {
            let encrypt = Confirm::with_theme(&ColorfulTheme::default())
                .with_prompt("Encrypt this upload?")
                .default(false)
                .interact()
                .map_err(|err| format!("failed to read confirmation: {err}"))?;
            if !encrypt {
                return Ok(None);
            }
        }
        match self.store.prompt_select_encryption_key() {
            Ok(key) => Ok(Some(key.alias.clone())),
            Err(message) => {
                println!("{message}");
                Ok(None)
            }
        }
    }

    fn non_interactive_fallback(&self) -> Result<Option<String>, String> {
        Ok(self.bucket_default.clone())
    }
}
```

Three ways this resolves, in priority order (`WizardInput::resolve()`'s existing precedence -- unchanged):

1. **`--encryption-key <alias>` flag** -- always wins outright, regardless of any bucket default. The explicit override.
2. **Interactive, no flag** -- if the bucket has a default, asks to use it (default yes) or pick something else entirely (declining both yields no encryption for this run -- interactive override, either direction). If the bucket has no default, behaves exactly as originally decided in this ADR: ask "Encrypt this upload?", then select.
3. **Non-interactive, no flag** -- this is the actual fix: resolves to the bucket's default automatically (`Ok(self.bucket_default.clone())`) instead of always `Ok(None)`. A scripted/cron run against a bucket configured with a default key now gets encrypted uploads without needing `--encryption-key` on every invocation; a bucket with no default still silently skips encryption, unchanged.

### 4. `run_email_sync_job`'s `.enc`-suffix decision is unchanged

```rust
let encrypt = encryptor.is_some();
```

in `src/commands/job/email_sync/worker.rs` is untouched by this amendment -- `encryptor: Option<&Aes256GcmSivEncryptor>` remains the only source of truth at that point, however its alias was resolved.

## Consequences

- Non-interactive/scripted runs can now actually get encryption, via a bucket-level default, without repeating `--encryption-key` every time -- the gap this amendment exists to close.
- Encryption is still fundamentally a per-run outcome, never a hard requirement: a bucket with a default can still be uploaded to unencrypted by declining interactively or simply not being asked (non-interactive with no default). The "mixed plaintext/ciphertext objects in one bucket" risk from the original ADR-0025 Consequences still exists, but is now mitigated by default for anyone who sets one -- consistency requires no ongoing effort once a bucket's default is configured, rather than being entirely the user's responsibility every single run.
- `BucketConfig` gains back one field, this time storing a key reference rather than a bool -- `#[serde(default)]` keeps every existing `keyring.toml` entry (with or without the old `encrypt: bool`) valid.
- `keyring add`/`modify bucket` gain back one prompt, now selecting a key rather than a yes/no.

## Out of scope

- Any bucket-level *requirement* that uploads be encrypted -- declining (interactively) or omitting `--encryption-key` (non-interactively, for a bucket with no default) is always possible; this ADR only changes what the *default* answer is, never removes the choice.
- Propagating a bucket's default-key change to objects already uploaded under a different key (or unencrypted) -- unchanged from ADR-0025's original gap.

Implementation is part of this same task (small enough not to defer).
