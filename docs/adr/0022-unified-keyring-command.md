# ADR-0022: unify `email authenticate`/`dataops bucket-config` into `pigeon keyring`

- **Author**: Willow Finch ([@noisypigeon](https://github.com/noisypigeon)).
- **Date**: 2026-09-24.
- **Status**: Accepted.

## Context

`pigeon email authenticate`/`list` (`src/email/{cli,commands,identity,provider,credentials}.rs`) and `pigeon dataops bucket-config new/edit/remove` (`src/dataops/{cli,commands,store,credentials}.rs`) are two independently-implemented, nearly-parallel "register a secret under a local alias" wizards. Each has its own TOML-backed metadata store (`identities.toml` / `bucket-configs.toml`), its own OS-keychain service name (`"pigeon"` / `"pigeon-dataops"` — deliberately distinct per ADR-0009, specifically so a bucket-config and an email identity that happened to share an alias could never collide in the keychain), its own `read_secret`/TTY-fallback helper (duplicated verbatim between `email::commands`/`dataops::commands`, an explicit, acknowledged cost of ADR-0008's "keep email/dataops independent siblings" philosophy), and its own alias-resolution/`prompt_select` pattern.

The two aren't even at parity today: `dataops bucket-config` supports add (`new`), edit, and remove; `email` supports only add (`authenticate`) and `list` — there is no way to edit or remove an authenticated email identity via the CLI at all.

Real use of this CLI (building and running `pigeon job run email-sync`, ADR-0021) showed these two wizards side by side often enough that maintaining them as separate implementations no longer earns its keep. This ADR replaces both with one `pigeon keyring add/modify/delete/list` command group, sharing a single implementation across both "kinds" of secret (email identity, bucket-config): one on-disk store, one keychain service, one set of prompt helpers, globally-unique aliases across both kinds. This is a deliberate, explicit reversal of ADR-0008's "independent siblings" decision for this specific pair of concerns, and retires the `bucket-config` subgroup ADR-0017 introduced. Both ADR-0008 and ADR-0017 are left unedited as historical record, per this project's established convention (e.g. ADR-0019 superseding ADR-0011/0012, ADR-0021 superseding ADR-0007/0012/0014).

**A large structural consequence falls directly out of what's being replaced.** `EmailCommands` (`src/email/cli.rs`) has exactly two variants today: `Authenticate`, `List`. `DataopsCommands` (`src/dataops/cli.rs`) has exactly one: `BucketConfig`. Replacing every one of them leaves both enums empty. **`pigeon email` and `pigeon dataops` cease to exist as CLI command groups.** Only `pigeon keyring` and `pigeon job` remain at the top level. The underlying Rust modules that back `job::email_sync` internally — `email::provider`, `email::imap_client`, `email::sink`, `email::transform`, `dataops::client`, `dataops::location`, `dataops::transform`, `dataops::dedup` — are untouched; only each domain's CLI-facing `cli.rs`/`commands.rs`, and `email::identity`'s/`dataops::store`'s `Identity`/`Store` and `BucketConfig`/`Store` metadata-storage types, go away.

## Decision

### 1. New `pigeon keyring` command group; `email`/`dataops` CLI groups removed entirely

```rust
pub struct KeyringArgs { #[command(subcommand)] pub command: KeyringCommands }
pub enum KeyringCommands {
    Add(AddArgs),
    Modify { alias: Option<String> },   // interactive Select over the unified store when omitted
    Delete { alias: String },           // required, no interactive selection -- a direct one-liner
    List,
}
pub struct AddArgs { #[command(subcommand)] pub kind: Option<AddKind> }  // Option so bare `keyring add` is valid and reaches an interactive Email-or-Bucket prompt
pub enum AddKind {
    Email {
        email: String,
        alias: Option<String>,
        provider: Option<Provider>,
        host: Option<String>,
        port: Option<u16>,
    },
    Bucket {
        alias: Option<String>,
    },
}
```

`add` keeps nested subcommands (`add email` / `add bucket`) rather than a single flat command, specifically to preserve today's full non-interactive/scriptable operation for `email authenticate`: `email authenticate <EMAIL> --alias ... --provider ... --host ... --port ...` already supports running with zero prompts (secret piped via stdin), and losing that would regress existing automation for no reason. `modify`/`delete` don't get the same flag-driven treatment: `dataops bucket-config edit` is *already* interactive-only today — it always prompts every field, pre-filled with the current value as its default, with no flag to override any of them non-interactively — so `modify` simply continues that existing precedent for both kinds. No scriptability is lost there, because none existed for editing before this ADR either.

Every `AddKind::Email` field mirrors today's `EmailCommands::Authenticate` flags exactly (same names, same optionality, same defaulting rules — e.g. `provider` auto-detected from `email`'s domain when omitted, `alias` defaulting to a sanitized form of it). `AddKind::Bucket` mirrors `BucketConfigCommands::New` exactly too — which, re-read directly, turns out to only ever have had one flag, `alias`; `bucket`/`endpoint`/`access_key_id` have no flags at all today and are always `Input`-prompted unconditionally, with the secret always `read_secret`-prompted. So `AddKind::Bucket` carries only `alias` — adding flags for the other three fields would be new scriptability this ADR doesn't ask for, not a mirror of what exists. Existing scripts/muscle memory need only insert `keyring add email` / `keyring add bucket` in place of the old command names.

`src/cli.rs`'s top-level `Commands` enum drops `Email(EmailArgs)` and `Dataops(DataopsArgs)` entirely and gains `Keyring(KeyringArgs)`, alongside the existing `Job(JobArgs)` (ADR-0021).

### 2. One unified store: `src/keyring/store.rs`

```rust
#[derive(Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum KeyringEntry {
    Email {
        alias: String,
        email: String,
        provider: Provider,
        host: String,
        port: u16,
    },
    Bucket {
        alias: String,
        bucket: String,
        endpoint: String,
        access_key_id: String,
    },
}

pub struct Store { entries: Vec<KeyringEntry> }
```

Persisted as `keyring.toml` (replacing `identities.toml` and `bucket-configs.toml`), via serde's internally-tagged enum — a `kind = "email"` / `kind = "bucket"` discriminant field on each `[[entries]]` table — using the same `toml`-crate round-trip pattern (`toml::from_str`/`to_string_pretty`) both existing stores already use. `KeyringEntry::alias(&self) -> &str` is a small convenience accessor matching over both variants.

`Store::default_path()` reuses the existing `PIGEON_CONFIG_DIR`-env-var-override-then-OS-config-dir pattern — today duplicated verbatim between `email::identity::CONFIG_DIR_ENV_VAR`/`Store::default_path()` and `dataops::store::CONFIG_DIR_ENV_VAR`/`Store::default_path()` — collapsing to one definition here. The reason for that duplication (ADR-0008's independent-siblings philosophy) no longer applies once there's a single command group consuming it.

`contains_alias`/`find`/`find_mut`/`push`/`remove`/`iter`/`is_empty` mirror both existing stores' APIs one-for-one, now simply operating over every entry regardless of kind — this, not any extra validation logic, is what makes alias uniqueness global: `contains_alias` used by `add`'s collision check now sees both kinds at once. `prompt_select` (used by `modify` when no alias is given) lists every entry with a kind-labeled line, e.g. `[email] willow-personal (willow@gmail.com, gmail)` / `[bucket] backup (s3.example.com, my-bucket)` — the same `Select`-with-formatted-labels shape as today's two separate `prompt_select` implementations (`email::identity::Store::prompt_select`, `dataops::store::Store::prompt_select`), just over a mixed list.

### 3. One keychain service: `src/keyring/credentials.rs`

`SERVICE_NAME = "pigeon"` — today's email service name; dataops's `"pigeon-dataops"` retires. `set_secret`/`get_secret`/`delete_secret` merge the two existing near-identical implementations verbatim, keeping dataops's `NoEntry`-tolerant `delete_secret` (the stricter of the two — treats a missing keychain entry as success rather than an error, needed for a clean `keyring delete` on a hand-edited or already-partially-removed entry).

Merging service names is safe now specifically *because* alias uniqueness is enforced globally (§2): the entire reason dataops picked a separate service name in the first place — ADR-0009's "so a bucket-config and an email identity that happen to share an alias... can never collide" — is structurally moot once that collision can't occur.

### 4. `src/keyring/wizard.rs`: the shared implementation

The "single implementation, multiple uses" piece the user asked for. Two kind-agnostic helpers, each unifying what was deliberately duplicated before: `read_secret(prompt) -> Result<String, String>` (masked `dialoguer::Password` on a TTY, a plain piped-line read otherwise — merging `email::commands::read_secret`/`dataops::commands::read_secret`, which were byte-for-byte the same shape) and `confirm(prompt, default) -> Result<bool, String>` (TTY `Confirm` or a piped `y`/`n`/`yes`/`no` line otherwise — merging in `dataops::commands::confirm`; `email` had no confirm helper at all before this, since it never needed one without a `remove` command).

- **`add`**: dispatches on `AddKind` from the parsed subcommand when given (`email` branch reuses `Provider::detect`, `Provider::prompt_select`, and the existing `resolve_host_port` host/port-resolution logic verbatim, then `imap_client::verify_login` to confirm the credential works before persisting anything; `bucket` branch reuses `client::bucket_exists` the same way, with bucket/endpoint/access-key-id always `Input`-prompted as today). A bare `pigeon keyring add` (no `email`/`bucket` subcommand) prompts `Select` "Email or Bucket?" first, then runs the same branch with every field unset -- fully interactive either way. Both branches check the *unified* store's `contains_alias` before proceeding, then call `credentials::set_secret` followed by `Store::push`+`save`, preserving the existing rollback-the-keychain-secret-if-the-metadata-write-fails safety net both originals already have.
- **`modify [ALIAS]`**: resolves the target via `Store::find` (alias given, erroring if unknown) or `Store::prompt_select` (omitted), then dispatches on the resolved entry's variant. The bucket branch is `dataops::commands::edit` unchanged in shape (bucket/endpoint/access-key-id/secret prompts pre-filled with current values via `.default(current.field.clone())`, "press enter to keep current" for the secret, re-running `bucket_exists` before saving). The email branch is genuinely new functionality — provider/host/port/secret prompts pre-filled the same way, re-running `verify_login` before saving — since no edit capability for identities exists anywhere in this codebase today.
- **`delete <ALIAS>`**: `Store::find` (error if the alias doesn't exist), `confirm("Remove '<alias>'?", false)` (kept as a safety prompt per explicit decision — the alias being passed directly on the command line doesn't waive confirmation, matching `dataops bucket-config remove`'s existing behavior exactly), then `Store::remove`+`save` and `credentials::delete_secret`.
- **`list`**: one `commands::print_table` call with a `KIND` column added ahead of `ALIAS`, replacing both `email list`'s table and `dataops`'s `list_bucket_configs` (which exists today as a plain function with no CLI command reaching it at all, per ADR-0017).

### 5. Consequences for `job::email_sync`

`src/job/email_sync.rs` (ADR-0021) currently imports `email::identity::{Identity, Store}` — for building `IdentityContext`s and for `wizard::resolve_identities` — and `dataops::store::BucketConfig` plus `dataops::credentials` for the upload-target lookup. Both become lookups against `keyring::store::{KeyringEntry, Store}` instead, filtered to the relevant variant (`KeyringEntry::Email` for identity resolution, `KeyringEntry::Bucket` for the upload target). `email::provider::Provider` and `email::identity::sanitize_segment`/`sanitize_alias` (used for mailbox-path and filename sanitization — unrelated to where credentials are stored) are untouched, along with every other library module `job` depends on.

This rewiring is real and necessary — Rust's whole-crate compilation means it has to land in the same change as §2/§3, not as a follow-up, exactly as ADR-0021's own `transform_one` rewrite had to land together with removing `email::sync.rs` rather than after it — but it's mechanical, not a design decision, so it isn't specified further here.

## Consequences

- `pigeon email authenticate`, `pigeon email list`, and `pigeon dataops bucket-config {new,edit,remove}` are removed — breaking, no migration shim, consistent with this project's established pattern (ADR-0016/0017/0021 precedent).
- `pigeon email` and `pigeon dataops` disappear entirely as CLI command groups, since each had nothing left once their sole remaining commands are replaced. `src/email/{cli,commands}.rs` and `src/dataops/{cli,commands}.rs` are deleted outright; every other module in both domains (sink, transform, imap_client, provider, client, location, dedup, transform) survives untouched, now serving only as internal plumbing for `job`.
- **Existing users' `identities.toml`/`bucket-configs.toml` files and their `"pigeon"`/`"pigeon-dataops"` keychain secrets are orphaned.** `pigeon keyring list` shows nothing until every identity and bucket-config is re-added via `pigeon keyring add`. This is a materially larger cost than prior renames in this project's history (ADR-0016/0017) — re-entering IMAP app passwords and S3 access keys, not just typing an updated command name — and is stated plainly here rather than glossed over.
- Alias uniqueness becomes global across both kinds going forward. A pre-existing collision from *before* this ADR (an identity and a bucket-config that happened to already share an alias) isn't something this change can detect, since it never reads or migrates the old files — moot in practice given the point above, but worth naming for completeness.
- `email`/`dataops`'s previously-duplicated `read_secret`/`confirm`/`CONFIG_DIR_ENV_VAR` collapse into one definition each — a direct, intended reversal of ADR-0008's "independent siblings, a little duplication is an acceptable cost" stance, scoped specifically to this pair of concerns.
- `job::email_sync.rs` requires the mechanical rewiring described in §5 as part of this same change, not a follow-up.

## Out of scope

- Migrating existing `identities.toml`/`bucket-configs.toml` data or keychain entries into the new unified store — explicitly not built, per the orphaned-data consequence above.
- Any change to `job run email-sync`'s own CLI surface or wizard flow (ADR-0021, amended for local-output/remote-output prompts) beyond the mechanical store-type rewiring in §5.
- A flag-driven, non-interactive `modify` or `delete` — both stay interactive-first for both kinds, matching (and, for email, introducing for the first time) `bucket-config edit`'s existing interactive-only precedent.
- Any change to how `job run email-sync` itself discovers configured bucket-configs to upload to, beyond reading them from the new unified store instead of the old one.

Implementation is a separate, later task.
