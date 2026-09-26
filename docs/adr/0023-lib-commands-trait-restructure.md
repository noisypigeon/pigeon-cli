# ADR-0023: restructure into `service/pigeon-cli/src/core/` (traits) + `service/pigeon-cli/src/commands/` (implementations)

- **Author**: Willow Finch ([@noisypigeon](https://github.com/noisypigeon)).
- **Date**: 2026-09-24.
- **Status**: Accepted.

## Context

This project's module layout has grown organically, one command group at a time: ADR-0008 established one folder per domain (`service/pigeon-cli/src/email/`, later `service/pigeon-cli/src/dataops/`) with a thin `cli.rs`/`commands.rs` roll-up in each; ADR-0021 added `service/pigeon-cli/src/job/` as a sibling, explicitly depending on `email`/`dataops` internals directly rather than through any shared interface; ADR-0022 added `service/pigeon-cli/src/keyring/` the same way, unifying `email`'s and `dataops`'s identity/bucket-config storage into one store and one keychain service. Four command-group-shaped folders now exist, each hand-rolling its own version of a few recurring shapes: "a configured secret with an alias" (identity vs. bucket-config), "a job that gathers work and runs it" (only email-sync exists, but the shape is there), "resolve this input from a flag, else prompt, else fall back or error" (five independent implementations across `job::wizard`), and "parse/transform content, dedup it against a durable index" (email message transform + `dataops`'s already-partially-genericized transform/dedup primitives, per ADR-0020).

This ADR restructures the codebase around that recognition: a `service/pigeon-cli/src/core/` layer holding trait definitions for these four shared concepts (`keyring`, `job`, `wizard`, `data`) plus whatever infrastructure under each is genuinely kind-agnostic, and a `service/pigeon-cli/src/commands/` layer holding the concrete CLI dispatch and per-kind implementations that fulfill those traits. `service/pigeon-cli/src/dataops` and `service/pigeon-cli/src/email` are removed outright; every file they contain moves into either `service/pigeon-cli/src/core/data` (the genuinely generic pieces already identified by ADR-0020) or `service/pigeon-cli/src/commands/keyring/{email,bucket}`/`service/pigeon-cli/src/commands/job/email_sync` (the concrete, kind-specific pieces).

**Naming note, discovered only once implementation started:** the request's literal `service/pigeon-cli/src/lib/keyring` etc. can't actually be declared that way — a module named `lib` cannot be a submodule of `service/pigeon-cli/src/lib.rs` itself (`error[E0761]`: the crate-root file and a `mod lib` both resolve to a file named `lib.rs`, an unconditional conflict, not a style choice). Offered three ways out (`service/pigeon-cli/src/shared/`, `service/pigeon-cli/src/core/`, or a `#[path]` attribute keeping the on-disk name `service/pigeon-cli/src/lib/` while the module is spelled differently in code); the user chose **`service/pigeon-cli/src/core/`**. Every `service/pigeon-cli/src/lib/`/`lib::`-prefixed reference below refers to what is actually `service/pigeon-cli/src/core/`/`core::` on disk.

This reverses two standing decisions, both named here rather than silently overridden. First, ADR-0008's "one folder per command group, no shared abstraction layer" convention — superseded by the two-layer split below. Second, ADR-0021's explicit choice *not* to build a generic job-type abstraction, reasoned at the time from "only one job type exists, and no second one is planned" — the `Job` trait introduced here is confirmed, on direct question, as an intentional consistency/extensibility choice made *despite* that reasoning still being true today (no second job type exists or is planned), not a response to new evidence that one is needed. Both prior ADRs stay unedited as historical record, per this project's established convention (e.g. ADR-0019 superseding ADR-0011/0012, ADR-0021 superseding ADR-0007/0012/0014).

A second question was resolved directly with the user before writing this decision: should `KeyringEntry` dispatch through true trait objects (`Box<dyn KeyringEntry>`, genuinely pluggable, but needing a new dependency like `typetag` since `serde` can't serialize trait objects on its own) or stay a concrete enum with the trait describing shared behavior only (no new dependency, consistent with the "no speculative plugin registry" position that still holds for extensibility-without-a-second-real-case)? → **Concrete enum, trait for behavior only.**

**A layering refinement, found while designing this concretely rather than staying at the level of the user's literal phrasing:** putting `KeyringEntry`'s concrete storage enum in `service/pigeon-cli/src/core/keyring` alongside the trait would force the abstraction layer to name concrete types owned by the implementation layer (`commands::keyring::email::Identity`, `commands::keyring::bucket::BucketConfig`) — inverting the dependency direction a `core`/`commands` split is supposed to establish. Resolution: `service/pigeon-cli/src/core/*` holds *only* trait definitions plus infrastructure that is genuinely kind-agnostic (needs to know nothing about which concrete kinds exist); anything that must name both concrete kinds — the storage enum, its serde tagging, `Store` itself — lives in `service/pigeon-cli/src/commands/keyring/store.rs` instead, since that's the layer allowed to know about both. (The concrete storage enum is also named `Entry`, not `KeyringEntry` — the trait already claims that name, and the enum lives one layer down.)

## Decision

### 1. New top-level layout

```
service/pigeon-cli/src/
  lib.rs, main.rs, cli.rs        -- unchanged in spirit; cli.rs's Commands enum now points at service/pigeon-cli/src/commands/{keyring,job}
  core/
    mod.rs                       -- pub mod data; pub mod job; pub mod keyring; pub mod wizard;
    keyring/
      mod.rs                     -- trait KeyringEntry { alias(), kind(), detail() }; pub mod credentials
      credentials.rs              -- set_secret/get_secret/delete_secret (moved from service/pigeon-cli/src/keyring/credentials.rs, unchanged)
    job.rs                       -- trait Job { type Plan; type Summary; async fn gather(&self) -> Result<Self::Plan, String>; async fn run(self, plan: Self::Plan, concurrency: usize) -> Result<Self::Summary, String>; }
    wizard.rs                    -- trait WizardInput (see §4); shared read_secret()/confirm() functions (moved from service/pigeon-cli/src/keyring/wizard.rs's helpers)
    data.rs                      -- trait Transform, trait Dedup (see §5); ContentIndex, unique_path, sanitize_filename, yaml_quote, collect_files, amend_frontmatter_for_duplicate, rewrite_attachment_reference (moved from service/pigeon-cli/src/dataops/{dedup,transform}.rs, unchanged apart from ContentIndex::commit dropping its redundant staging_dir parameter)
  commands/
    mod.rs                       -- top-level dispatch, print_table, FAILURE_EXIT_CODE (unchanged)
    keyring/
      mod.rs, cli.rs, commands.rs -- unchanged shape (KeyringArgs/KeyringCommands/AddArgs/AddKind, dispatch)
      store.rs                    -- concrete Entry enum (Email(Identity)/Bucket(BucketConfig)) + Store, each variant implementing core::keyring::KeyringEntry
      wizard.rs                   -- add/modify/delete/list handlers (moved from service/pigeon-cli/src/keyring/wizard.rs), built on core::wizard's shared pieces
      email/
        mod.rs, identity.rs        -- Identity, sanitize_segment, sanitize_alias
        provider.rs                -- Provider enum (moved from service/pigeon-cli/src/email/provider.rs, unchanged)
        imap_client.rs             -- connect_and_login/verify_login (moved from service/pigeon-cli/src/email/imap_client.rs, unchanged)
      bucket/
        mod.rs, store.rs           -- BucketConfig struct (moved from service/pigeon-cli/src/dataops/store.rs)
        client.rs                  -- S3 operations: bucket_exists, upload_if_changed, list_buckets, list_objects, get_object (moved from service/pigeon-cli/src/dataops/client.rs, unchanged)
    job/
      mod.rs, cli.rs, commands.rs -- unchanged shape (JobArgs/JobCommands/RunArgs/JobType, dispatch)
      email_sync/
        mod.rs                     -- IdentityContext/IdentityManifestSummary/PendingMailbox/gather_pending/batches_from_pending; EmailSyncJob + EmailSyncPlan implementing core::job::Job
        wizard.rs                  -- the five WizardInput impls + dispatch/dispatch_async orchestration (moved from service/pigeon-cli/src/job/email_sync.rs's dispatch/dispatch_async + service/pigeon-cli/src/job/wizard.rs)
        manifest.rs                -- ManifestEntry/CheckpointEntry/Batch/split_into_batches (moved from service/pigeon-cli/src/job/manifest.rs, unchanged)
        sink.rs                    -- fetch_uids, mailbox sanitization, UIDVALIDITY tracking (moved from service/pigeon-cli/src/email/sink.rs, unchanged)
        transform.rs               -- EmailTransform: parses .eml, renders markdown+frontmatter; implements core::data::Transform (moved from service/pigeon-cli/src/email/transform.rs)
        dedup.rs                   -- EmailDedup / the post-transform dedup pass; implements core::data::Dedup (moved from job::email_sync.rs's run_dedup_pass, built on core::data::ContentIndex)
        worker.rs                  -- run_worker/WorkerConnection/connect_with_retry/retry_with_backoff/run_email_sync_job (moved from service/pigeon-cli/src/job/email_sync.rs)
```

**Naming correction from the request as phrased**: `service/pigeon-cli/src/commands/job/email-sync` is not a valid Rust module path — module directory names must be valid Rust identifiers, and identifiers can't contain hyphens. The directory is `email_sync` (underscore); the CLI-facing subcommand string is unaffected and stays `email-sync` (clap already renders the subcommand name independently of the Rust module name, exactly as it does today).

### 2. `service/pigeon-cli/src/core/keyring`: the `KeyringEntry` trait

```rust
/// Behavior shared by every kind of secret this CLI manages under one
/// keyring.toml + one keychain service (ADR-0022).
pub(crate) trait KeyringEntry {
    fn alias(&self) -> &str;
    fn kind(&self) -> &'static str;   // "email" / "bucket" -- matches the serde tag value
    fn detail(&self) -> String;       // one-line summary for `list`/`prompt_select` labels
}
```

(`pub(crate)`, not `pub` -- every trait introduced by this ADR ended up scoped that way; see the implementation note at the end of this section.)

`commands::keyring::email::Identity` and `commands::keyring::bucket::BucketConfig` each `impl KeyringEntry`. The concrete `Entry` enum in `commands/keyring/store.rs` (`#[serde(tag = "kind")] enum Entry { Email(Identity), Bucket(BucketConfig) }`, per the confirmed enum-not-trait-object choice) implements `KeyringEntry` too, by matching and delegating to whichever variant is inside — this is what lets `Store::prompt_select`/`list` operate generically over the trait instead of matching on the concrete enum directly at every call site.

### 3. `service/pigeon-cli/src/core/job`: the `Job` trait

```rust
/// Behavior shared by every job type this CLI can run. Exactly one
/// implementor exists today (`commands::job::email_sync::EmailSyncJob`) --
/// introduced now per explicit direction, understood as a consistency/
/// extensibility choice rather than a response to a second job type
/// actually existing (ADR-0021 deliberately scoped that out; see
/// Consequences).
pub(crate) trait Job {
    type Plan;
    type Summary;
    /// Discover pending work without doing any of it (ADR-0021 §3/§4),
    /// returning what it found rather than discarding it -- `gather`'s
    /// caller needs the manifest summary and pending-work list to display
    /// the pre-run summary and resolve concurrency before `run` starts.
    async fn gather(&self) -> Result<Self::Plan, String>;
    /// Execute at the given concurrency, after the wizard's confirm step,
    /// consuming `self`'s `gather()` result.
    async fn run(self, plan: Self::Plan, concurrency: usize) -> Result<Self::Summary, String>;
}
```

**Refinement found during implementation, not present in the version of this section originally drafted:** the sketch above elided `gather`'s real return type as "an implementation-time detail." Concretely working it out surfaced that a bare `Result<(), String>` genuinely can't work -- `gather`'s caller needs what it discovered (the pending-mailbox list and manifest summary) to feed into `run`, not just a success/failure signal. Real shape: a `Plan` associated type, threaded from `gather` into `run`. `Job` is also never boxed as `dyn Job` (only ever used as the concrete `EmailSyncJob`), so native `async fn` in the trait (stable since Rust 1.75) needs no `async-trait` crate and never runs into AFIT's dyn-compatibility limitation.

### 4. `service/pigeon-cli/src/core/wizard`: the `WizardInput` trait, and shared prompt helpers

The best-motivated of the four traits: the "resolve from a flag, else prompt on a TTY, else error-or-default" shape already appears five times in the current codebase (`resolve_identities`, `resolve_local_output`, `resolve_remote_output`, `resolve_concurrency`, `confirm_and_proceed`, all in `service/pigeon-cli/src/job/wizard.rs`), independently hand-written each time with the same `is_terminal()`-gated structure.

```rust
pub(crate) trait WizardInput {
    type Value;
    /// `None` if the flag was omitted (fall through to `prompt`/
    /// `non_interactive_fallback`); `Some(Err(_))` if it was given but
    /// invalid (surfaced immediately -- an invalid flag never falls through
    /// to an interactive prompt, e.g. an unknown identity alias). Named
    /// `flag_value`, not `from_flag` -- clippy's `wrong_self_convention`
    /// lint reserves `from_*` names for associate functions that take no
    /// `self`.
    fn flag_value(&self) -> Option<Result<Self::Value, String>>;
    fn prompt(&self) -> Result<Self::Value, String>;
    /// `Err` = required (missing outside a TTY is a hard error, per
    /// ADR-0021 §8's narrow-`--yes` reasoning); `Ok(default)` = optional
    /// (missing outside a TTY silently falls back to a prior default, per
    /// the local-output/remote-output amendment to that same section).
    fn non_interactive_fallback(&self) -> Result<Self::Value, String>;
    /// The shared resolution algorithm every wizard input follows, given
    /// once here so implementors only define the three methods above.
    fn resolve(&self) -> Result<Self::Value, String> {
        if let Some(result) = self.flag_value() {
            return result;
        }
        if std::io::stdin().is_terminal() {
            self.prompt()
        } else {
            self.non_interactive_fallback()
        }
    }
}
```

**Refinement found during implementation:** the sketch originally drafted for this section returned a bare `Option<Self::Value>` from the flag-check method. That can't represent "a flag was given but is invalid" (e.g. an identity alias that doesn't exist) as anything other than silently falling through to an interactive prompt -- wrong; today's hand-written `resolve_identities` returns that error immediately, never prompting. Real shape returns `Option<Result<Self::Value, String>>`, and a default `resolve()` method (shown above) captures the shared flag-else-prompt-else-fallback algorithm once, so each of the five concrete inputs only implements the three small methods.

`read_secret(prompt: &str) -> Result<String, String>` and `confirm(prompt: &str, default: bool) -> Result<bool, String>` move here as plain shared functions, not trait methods — there is exactly one real implementation of "how do you read a secret" anywhere in this codebase, so a trait would be pure ceremony around it. The genuinely-varying concept is the flag-vs-prompt-vs-fallback *shape* each wizard input follows, which is what `WizardInput` captures.

### 5. `service/pigeon-cli/src/core/data`: the `Transform`/`Dedup` traits

```rust
pub(crate) trait Transform {
    type Input;
    type Output;
    fn transform(&self, input: Self::Input) -> Result<Option<Self::Output>, String>;
}

pub(crate) trait Dedup {
    fn check(&self, hash: &str) -> Option<&str>;
    fn commit(&mut self, hash: &str, relative_path: &str) -> Result<(), String>;
}
```

`commands::job::email_sync::transform::EmailTransform` implements `Transform`, wrapping today's `transform_one` logic (parse `.eml`, render markdown/frontmatter, stage to the UID-keyed tree per ADR-0021 §10's addendum). `commands::job::email_sync::dedup::EmailDedup` implements `Dedup` by wrapping a `core::data::ContentIndex` and forwarding both methods straight through -- `ContentIndex` itself doesn't need to change or become trait-based, since it's already a generic, reusable concrete type per ADR-0020 with no per-kind variation to abstract over; only the *decision* of what to do with a hash-check result (merge a duplicate message vs. reuse a duplicate attachment) varies by kind, which is exactly what `Dedup`'s two methods leave to the implementor. `unique_path`, `sanitize_filename`, `yaml_quote`, `collect_files`, `amend_frontmatter_for_duplicate`, and `rewrite_attachment_reference` move here unchanged as plain shared functions, for the same reason `read_secret`/`confirm` stay plain functions in `core::wizard` — none of them vary per kind, so wrapping them in a trait would add indirection without adding meaning. `ContentIndex::commit` also drops a redundant `staging_dir: &Path` parameter it previously took on every call despite it never changing after `load()` -- stored as a field instead, which is what lets its signature match `Dedup::commit(&mut self, hash, relative_path)` exactly.

**Why every trait here ended up `pub(crate)`:** `async fn` in a `pub` trait triggers clippy's `async_fn_in_trait` lint (missing `Send` bounds needed for a `dyn`-safe public API). None of these five traits (`Job`, `KeyringEntry`, `WizardInput`, `Transform`, `Dedup`) have or need external consumers -- this crate has no `lib` target used by anything outside itself -- so `pub(crate)` is both the correct scope on its own merits and what resolves the lint.

### 6. `service/pigeon-cli/src/dataops` and `service/pigeon-cli/src/email` are removed

Every file's destination is named in §1's tree above. Nothing is deleted outright — every current file's logic has a new home; this ADR is a reorganization plus the new trait layer described in §2–§5, not a functional rewrite. No on-disk format, CLI flag, or observable behavior changes as a result of this ADR by itself.

## Consequences

- `Job` and `Transform`/`Dedup` are each introduced with exactly one real implementor today. Their abstraction value is currently nominal — no second job type, no second kind of content to transform or dedup exists or is planned. This is an explicit, acknowledged cost of prioritizing structural consistency (all four "shared concepts" getting the same treatment) over this project's usual YAGNI instinct, accepted via direct confirmation from the user for the `Job` case specifically, where the tension with ADR-0021's prior reasoning is most direct.
- `WizardInput` is the one trait with real, non-speculative motivation today (five existing hand-written instances of the exact pattern it captures). Retrofitting those five call sites to actually implement it, rather than just continuing to hand-write the same shape a sixth and seventh time, is real, non-trivial work — not just a rename.
- `KeyringEntry` staying enum-backed (not `Box<dyn>`) means a hypothetical third keyring kind would still require touching the central `Entry` enum in `commands/keyring/store.rs` rather than being added purely by implementing a trait elsewhere — consistent with this project's standing position against speculative plugin architecture (ADR-0021's out-of-scope list), but worth naming plainly as the genuine tradeoff against true pluggability.
- `commands::job::email_sync` depends on both `commands::keyring::email` (for `Identity`/`Provider`/IMAP authentication) and `commands::keyring::bucket` (for `BucketConfig`/S3 upload) — the same dependency shape `job` already has on `email`/`dataops` today, just pointing at the new paths. `job` remains the top of this crate's internal dependency graph.
- This is a large mechanical move — most files relocate with their logic otherwise unchanged — plus a genuinely new trait layer for `Job`, `Transform`, `Dedup`, and `KeyringEntry`. No user-visible CLI behavior changes: same commands, same flags, same output, same on-disk formats.

## Out of scope

- Turning the bucket/S3 client operations (`commands::keyring::bucket::client`) into a trait — the request named transform/dedup and email/bucket specifically, not the S3 wire protocol; those operations stay concrete, non-trait functions. ([#27](https://github.com/noisypigeon/pigeon/issues/27))
- Any actual new job type, keyring kind, transform strategy, or dedup strategy — this ADR only introduces the trait layer and relocates existing logic to implement it against that layer.
- Any change to on-disk formats (`keyring.toml`, `.job-checkpoint`, `.manifest`, the dedup dotfiles) or any CLI-visible behavior.

Implementation is a separate, later task.
