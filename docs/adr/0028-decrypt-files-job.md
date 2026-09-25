# ADR-0028: `pigeon job run decrypt-files`

- **Author**: Willow Finch ([@noisypigeon](https://github.com/noisypigeon)).
- **Date**: 2026-09-24.
- **Status**: Accepted.

## Context

ADR-0025 added `Encryptor::decrypt` "for symmetry, testability, and so a future download/restore command has it ready," explicitly scoping out any CLI caller: "no CLI command calls this yet... surfacing it is a separate ADR when download/restore tooling exists." It's carried an `#[allow(dead_code)]` (`src/core/crypto.rs`) ever since -- nothing in this codebase has ever called it outside its own round-trip unit tests. ADR-0026 and ADR-0027 built out full encryption-key management (`pigeon keyring add/modify/delete encryption-key`) and per-run key selection, but only for the upload direction.

This ADR is that deferred follow-up: a new job, `pigeon job run decrypt-files`, mirroring `email-sync`'s wizard-driven, `core::job::Job`-trait-based shape (`src/core/job.rs`), to reverse encryption applied at upload time. Given a local directory of already-downloaded `*.enc` files and an encryption-key alias, it decrypts every one back to plaintext under an output directory, mirroring the input tree with the `.enc` suffix stripped. This is a purely local, one-way "get my plaintext back" operation -- it does not talk to any bucket itself.

## Decision

### 1. `pigeon job run decrypt-files --input-dir <path> --output-dir <path> --encryption-key <alias> [--concurrency N] [--yes]`

New `JobType::DecryptFiles` variant (`src/commands/job/cli.rs`), alongside the existing `EmailSync`:

```rust
DecryptFiles {
    #[arg(long)]
    input_dir: PathBuf,

    #[arg(long)]
    output_dir: PathBuf,

    #[arg(long)]
    encryption_key: Option<String>,

    #[arg(long)]
    concurrency: Option<usize>,

    #[arg(long)]
    yes: bool,
},
```

`--input-dir`/`--output-dir` are required flags (unlike `email-sync`'s optional, defaulted ones) -- there's no sane default location for either side of a decrypt operation the way there is for `email-sync`'s temp-dir-backed staging tree. Before any work starts, the job fails fast if the two paths canonicalize to the same directory -- decrypting into the tree it reads from risks data loss if the run fails partway through.

### 2. New module `src/commands/job/decrypt_files/`

Mirrors `email_sync`'s three-way split (`src/commands/job/email_sync/{mod,wizard,worker}.rs`):

- **`mod.rs`**: `DecryptFilesJob: Job` (the second real implementor of `core::job::Job` -- today it has exactly one, `EmailSyncJob`, per that trait's own doc comment describing its single-implementor status as a deliberate consistency/extensibility choice; this ADR is the first time that choice actually pays off) and the shared `DecryptTask`/`DecryptSummary` types.
- **`wizard.rs`**: CLI dispatch (`pub fn dispatch(...)`, spinning up its own `tokio` runtime exactly like `email_sync::wizard::dispatch` does) plus the `WizardInput` impls below.
- **`worker.rs`**: the actual decrypt work.

### 3. `DecryptFilesJob::gather`/`run`

```rust
struct DecryptTask {
    input_path: PathBuf,
    output_path: PathBuf,
}

pub(crate) struct DecryptFilesJob {
    pub input_dir: PathBuf,
    pub output_dir: PathBuf,
    pub encryptor: Aes256GcmSivEncryptor,
}

impl Job for DecryptFilesJob {
    type Plan = Vec<DecryptTask>;
    type Summary = DecryptSummary;

    async fn gather(&self) -> Result<Vec<DecryptTask>, String> {
        // core::data::collect_files(&self.input_dir), filtered to files whose
        // extension is "enc"; output_path = output_dir.join(relative path
        // with the .enc extension stripped via Path::with_extension("")).
    }

    async fn run(self, plan: Vec<DecryptTask>, concurrency: usize) -> Result<DecryptSummary, String> {
        // worker::run_decrypt_phase(plan, &self.encryptor, concurrency).await
    }
}
```

`gather()` prints a short "N encrypted file(s) found" summary (mirroring `print_manifest_summary`'s role for `email-sync`) before the concurrency/proceed prompts. `run()` decrypts every task concurrently via `stream::buffer_unordered(concurrency)`, mirroring `run_upload_phase`'s exact shape (`src/commands/job/email_sync/worker.rs`): read the ciphertext, `encryptor.decrypt(&data)`, create the output file's parent directory if needed, write the plaintext. Progress bar via the existing `email_sync::sink::new_progress_bar` (`pub(crate)`, already reusable cross-module within the crate -- no relocation needed for a second caller).

### 4. Wizard inputs (`decrypt_files::wizard`)

- **`InputDirInput`/`OutputDirInput`**: simple required-path prompts (`Input::<String>::new().with_prompt(...)`), erroring non-interactively if omitted (`--input-dir`/`--output-dir` given directly, since both are plain required `PathBuf` clap args, these two `WizardInput` impls are only exercised on the rare bare/malformed invocation -- kept for symmetry with every other input this job's wizard resolves, and so a future relaxation to optional-with-a-default stays a small change).
- **`EncryptionKeyInput`** (module-scoped, distinct from `email_sync`'s -- decrypt's version is *mandatory* and returns `String`, not `Option<String>`; there is no "skip encryption" case here, decrypting is the entire point of running this command): `--encryption-key` if given, else `store.prompt_select_encryption_key()` interactively (auto-selects the sole key if there's exactly one, errors clearly if there are none), else a hard non-interactive error.
- **`ConcurrencyInput`/`ConfirmInput`**: duplicated locally from `email_sync::wizard`'s exact shape rather than shared -- matches this codebase's established tolerance for small single-purpose duplication over premature abstraction (ADR-0023's own reasoning for `Job`/`Transform`/`Dedup` having one implementor apiece). Decrypt's `ConcurrencyInput` skips `email_sync`'s time-estimate table entirely -- that estimate is explicitly uncalibrated per ADR-0021 §9 for *fetch/transform* throughput specifically; fabricating a second uncalibrated number for a different (disk-bound decrypt) workload isn't worth it.

### 5. Per-file failures are warned about and counted, not fatal

A file that fails to decrypt -- wrong key, truncated, tampered (AEAD tag mismatch) -- is reported via `multi_progress.println` and counted in `DecryptSummary.failed`, without aborting the rest of the run. Same philosophy as upload's per-file failure handling (ADR-0024 §6), minus retry-with-backoff: this is local disk I/O, not a network call, so a decrypt failure is never transient -- retrying the same bytes against the same key produces the same result every time.

### 6. `Encryptor::decrypt`'s `#[allow(dead_code)]` is removed

`src/core/crypto.rs`'s trait method (added in ADR-0025, annotated since nothing called it) gets its first real, non-test caller here.

## Consequences

- `Encryptor::decrypt`'s round-trip guarantee -- already unit-tested since ADR-0025 -- is now exercised by real CLI usage, not just tests.
- Anyone who encrypted uploads (ADR-0025/0027) can get plaintext back without writing custom tooling against `core::crypto` themselves.
- `core::job::Job` gets a second real implementor for the first time, the first actual validation that the trait's genericity (`type Plan`/`type Summary`, `gather`/`run`) holds up across more than one job shape.
- `pigeon job run` now has two subcommands; `job/cli.rs`/`job/commands.rs` grow their first real `match` branch beyond `EmailSync`.

## Out of scope

- Any bucket-download step (e.g. new `pigeon dataops`/bucket "copy down" tooling) -- this ADR assumes the encrypted files are already present in `--input-dir` by whatever means (manual download, existing S3-compatible tooling, a future ADR). ([#33](https://github.com/noisypigeon/pigeon-cli/issues/33))
- Re-encrypting or re-uploading decrypted output -- this is a one-way "get my plaintext back" tool, not a re-encryption or migration utility.
- Any change to the encryption scheme, key management, or upload path -- ADR-0025/0026/0027 stand entirely as decided.

Implementation is a separate, later task.
