# ADR-0017: rename `remote` to `dataops`, restructure its CLI

- **Author**: Willow Finch ([@noisypigeon](https://github.com/noisypigeon)).
- **Date**: 2026-09-23.
- **Status**: Accepted.

## Context

`pigeon remote` (`service/pigeon-cli/src/remote/`) was introduced by ADR-0009 and refined by ADR-0010; ADR-0016 most recently touched its keychain code and `list` output. This ADR renames the module and its CLI surface to `dataops`, and restructures its commands: `configure`/`edit`/`remove` move under a new `bucket-config` subgroup, `list-buckets` is removed entirely, and `list`/`ls`/`lsd`/`copy` are removed as CLI subcommands but kept as internal, reusable functions rather than deleted.

Two scope questions were resolved directly with the user before writing this ADR:
- **`list`/`ls`/`lsd`/`copy`**: removed as CLI subcommands; their implementations stay in the `dataops` module as plain functions available to other code — e.g. `email`'s existing `--remote-output` upload path (ADR-0011), which already calls client-level functions directly rather than through CLI dispatch, and any future command that wants them.
- **Rename depth**: full — not just the module path and CLI group, but every internal `Remote`-rooted identifier, the on-disk config filename, and the OS-keychain service name too, accepting the resulting breaking changes. No migration shim, consistent with this project's established preference (e.g. ADR-0016's renames).

Full inventory of what "remote" touches today, confirmed by reading the current code:
- `service/pigeon-cli/src/remote/{cli,client,commands,credentials,location,mod,store}.rs`.
- Cross-module references: `service/pigeon-cli/src/cli.rs` (`Commands::Remote(RemoteArgs)`), `service/pigeon-cli/src/commands/mod.rs` (dispatch), `service/pigeon-cli/src/email/commands.rs` (`crate::remote::credentials as remote_credentials`, `crate::remote::store::{Remote, Store as RemoteStore}`), `service/pigeon-cli/src/email/sync.rs` (`crate::remote::client`, `crate::remote::store::Remote`).
- Internal naming: the `Remote` struct (`store.rs`), `remotes.toml`/`REMOTES_FILE_NAME`, `StoreFile.remotes: Vec<Remote>`, `Location::Remote { alias, path }` (`location.rs`), the keychain `SERVICE_NAME = "pigeon-remote"` (`credentials.rs`), and numerous "remote"-worded user-facing strings (`"no remote named '...'"`, `"a remote named '...' already exists"`, `"Select a remote"`, `prompt_select`'s `"run 'pigeon remote configure' first"`, etc.).
- `docs/adr/0009-remote-storage.md`, `0010-remote-storage-improvements.md`, and `0016-keychain-stability-and-cli-cleanup.md` all describe `pigeon remote`/`Remote` as they existed when written. Per this project's established convention, prior ADRs are not retroactively edited — they remain accurate historical records, and this ADR supersedes them going forward as a new, additive decision.

## Decision

### Module rename

`service/pigeon-cli/src/remote/` → `service/pigeon-cli/src/dataops/`, same internal file layout (`cli.rs`, `client.rs`, `commands.rs`, `credentials.rs`, `location.rs`, `mod.rs`, `store.rs`). Every `crate::remote::*` reference across the crate updates to `crate::dataops::*`.

### CLI group rename

`service/pigeon-cli/src/cli.rs`'s `Commands::Remote(RemoteArgs)` → `Commands::Dataops(DataopsArgs)`; `RemoteArgs` → `DataopsArgs`, `RemoteCommands` → `DataopsCommands` in `dataops/cli.rs`.

### New `bucket-config` subgroup

Replaces the flat `configure`/`edit`/`remove` commands: `DataopsCommands::BucketConfig(BucketConfigArgs)`, with `BucketConfigArgs { command: BucketConfigCommands }` and `BucketConfigCommands::{New, Edit, Remove}` — the same nested-subcommand clap pattern already used for `EmailArgs`/`RemoteArgs` today.

- `pigeon dataops bucket-config new [ALIAS]` (was `remote configure`)
- `pigeon dataops bucket-config edit [ALIAS]` (was `remote edit`)
- `pigeon dataops bucket-config remove [ALIAS]` (was `remote remove`)

### `list-buckets` removed entirely

**Correction found during implementation**: this point originally claimed ADR-0009's inline "list buckets with these credentials?" step inside `configure` reuses `client::list_buckets` and is unaffected by this removal. That's no longer accurate — ADR-0010 already removed that inline step from `configure`, replacing it with a `bucket_exists` check. So `client::list_buckets` has had **no caller at all** since ADR-0010, and removing the standalone `list-buckets` subcommand doesn't change that — it gets the same "kept but unused, preserved for reuse" treatment as `list`/`ls`/`lsd`/`copy` below, not a free pass via a still-live call site.

### `list`/`ls`/`lsd`/`copy` removed as CLI subcommands

Their implementations are kept as plain, non-clap-dispatched functions in the `dataops` module, available for reuse by other code.

### Full internal rename accompanying the module move

- `Remote` struct → `BucketConfig` — ripples through every type annotation that names it, including `email::sync::run`'s `output_remote: Option<(&Remote, &str)>` parameter type. Its CLI-facing flag name and internal variable name, both already settled by ADR-0016, are untouched; only the *type* changes.
- `remotes.toml` → `bucket-configs.toml`; `REMOTES_FILE_NAME` → `BUCKET_CONFIGS_FILE_NAME`; `StoreFile.remotes` → `StoreFile.bucket_configs` (so the TOML table header becomes `[[bucket_configs]]`).
- `Store as RemoteStore` import-alias convention → `Store as DataopsStore`.
- `Location::Remote { alias, path }` (`location.rs`) → `Location::Bucket { alias, path }`.
- `credentials.rs`'s keychain `SERVICE_NAME = "pigeon-remote"` → `"pigeon-dataops"`.
- User-facing strings ("no remote named...", "a remote named... already exists", "Select a remote", `prompt_select`'s "run 'pigeon remote configure' first", etc.) reworded to "bucket-config"/"bucket configuration" phrasing — exact wording is an implementation-time detail, not enumerated here.

### Prior ADRs stay unedited

No retroactive edits to ADR-0009, ADR-0010, or ADR-0016 — they remain accurate records of what was decided when written; this ADR is the new, superseding decision. `CLAUDE.md`'s existing one-line summaries for those ADRs stay as-is; a new bullet for ADR-0017 is added instead.

## Consequences

- Breaking CLI surface reduction: `pigeon remote list/ls/lsd/copy/list-buckets` no longer exist as commands. Their logic isn't deleted, just no longer clap-dispatched — available for a future command to re-expose.
- Breaking on-disk format change: an existing `remotes.toml` (`[[remotes]]`) is not read by the renamed `bucket-configs.toml` (`[[bucket_configs]]`) — every bucket-config must be re-created via `pigeon dataops bucket-config new`. No migration path, matching this project's established no-compatibility-shim preference.
- Breaking keychain change: renaming the keychain `SERVICE_NAME` from `"pigeon-remote"` to `"pigeon-dataops"` means existing stored secret access keys become unreachable under the new lookup key too — compounds the config-file break above; every bucket-config's secret must be re-entered, not just its metadata.
- `pigeon dataops`'s CLI surface narrows to pure configuration management (`bucket-config new/edit/remove`) even though the module/crate identity is now the broader "dataops" name — an accepted, intentional asymmetry per the user's explicit choice.
- `email`'s `--remote-output` upload path (ADR-0011) needs its imports and the `Remote` → `BucketConfig` type rename applied, but no behavioral change — it already calls client-level functions directly, not through CLI dispatch.

## Out of scope

- Re-exposing `list`/`ls`/`lsd`/`copy`/`list-buckets` as CLI commands anywhere else — not decided here. ([#24](https://github.com/noisypigeon/pigeon-cli/issues/24))
- Any migration/import path for an existing `remotes.toml` or its keychain entries — explicitly not provided. ([#23](https://github.com/noisypigeon/pigeon-cli/issues/23))
- Any change to the actual S3/MinIO client logic (`client.rs`'s API calls themselves) — this is a naming/surface reorganization only.
- Implementation itself — like every ADR before it, this is a decision record only.
