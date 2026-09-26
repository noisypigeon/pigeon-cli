# ADR-0016: keychain credential stability and `pigeon email` CLI cleanup

- **Author**: Willow Finch ([@noisypigeon](https://github.com/noisypigeon)).
- **Date**: 2026-09-23.
- **Status**: Accepted.

## Context

Two kinds of problems, decided together in one ADR.

**Fix: remote secrets lose keychain access after every recompile**, requiring the remote to be deleted and re-added. Investigated directly: `service/pigeon-cli/src/remote/credentials.rs` and `service/pigeon-cli/src/email/credentials.rs` use a structurally **identical** pattern — `keyring::Entry::new(SERVICE_NAME, alias)` plus `set_password`/`get_password`/`delete_credential` — differing only in the `SERVICE_NAME` constant (`"pigeon"` vs `"pigeon-remote"`, per ADR-0003/ADR-0009). There is no code-level asymmetry between the two paths to find. `Cargo.lock` confirms `keyring = "4.2.0"` uses the newer `apple-native-keyring-store` backend on macOS, and `.mise.toml`'s `build`/`pigeon` tasks do zero code-signing — just plain `cargo build` / `cargo run --`. On macOS, an unsigned binary gets an ad-hoc code signature auto-applied at link time, and that signature's identity changes on every rebuild (the binary's bytes differ each time). macOS Keychain access grants for items created via the Security framework are tied to the creating process's code identity, so a freshly rebuilt binary is treated as a different, untrusted application — a well-known class of macOS dev-tooling friction. This explains the reported symptom precisely and, critically, would affect **both** `email` and `remote` keychain entries equally. The most likely reason only remotes have shown it so far is that remote credentials were exercised across far more rebuilds during today's ADR-0009–0011 work than email identities were, not a structural difference between the two. The fix targets the actual root cause — unstable code identity across rebuilds — not either credential module's code.

**Refactor**, all in `pigeon email`'s CLI surface:
- `pigeon email list-identities` → `pigeon email list`, matching `pigeon remote list`'s already-established naming (`service/pigeon-cli/src/remote/cli.rs`) — amends ADR-0001's original naming choice.
- `pigeon email sync`'s `--staging-dir`/`--output-dir` (both currently required, `service/pigeon-cli/src/email/cli.rs`) collapse into one `--local-output <DIR>`, with the code deriving `staging` and `result` subdirectories underneath — automating the exact by-hand convention already in use (`.../staging`, `.../result`).
- `--output-remote` → `--remote-output`, for naming symmetry with `--local-output`.
- When `--local-output` is omitted, default to `/tmp/{alias}/`, implemented via `std::env::temp_dir().join(&identity.alias)` — stdlib, no new dependency, and identical to the literal request on macOS/Linux while staying portable.
- `pigeon email list` and `pigeon remote list` gain column headers and aligned-table output, replacing today's unlabeled, tab-separated rows.

## Decision

### Stable ad-hoc code signature across rebuilds

`.mise.toml`'s `build` and `pigeon` tasks gain a post-build step: `codesign --force -s - --identifier <stable-id> target/debug/pigeon` (macOS-only; a no-op elsewhere, since Linux's Secret Service backend has no equivalent per-app-signature ACL model to destabilize). This keeps every rebuilt binary presenting the same code identity to Keychain, so an "Always Allow" grant survives future rebuilds instead of being invalidated by the next `cargo build`. No changes to `email::credentials` or `remote::credentials` themselves — confirmed above that neither is where the bug lives.

### `pigeon email list-identities` → `pigeon email list`

`EmailCommands::ListIdentities` becomes `EmailCommands::List` in `service/pigeon-cli/src/email/cli.rs` (clap kebab-cases the variant name automatically, no extra attribute needed).

### `pigeon email sync`: `--local-output <DIR>` replaces `--staging-dir`/`--output-dir`

`--local-output` becomes an optional `PathBuf` in `service/pigeon-cli/src/email/cli.rs`. `service/pigeon-cli/src/email/commands.rs::sync()` resolves it to `staging_dir = local_output.join("staging")` and `output_dir = local_output.join("result")` once the identity has been resolved (the alias-based default below needs it), applied uniformly across the default flow and both `--debug sink`/`--debug transform` modes.

### `--output-remote` → `--remote-output`

Same `Option<String>` semantics and existing `--debug`-combination rejection behavior in `commands.rs` — renamed only.

### Default `--local-output`

When omitted, `--local-output` defaults to `std::env::temp_dir().join(&identity.alias)` — the portable equivalent of the requested `/tmp/{alias}/`.

### `email list`/`remote list` become headered tables

A small shared helper (e.g. `crate::commands::print_table`) is added to `service/pigeon-cli/src/commands/mod.rs` — the existing thin top-level roll-up both `email::commands` and `remote::commands` already pull `FAILURE_EXIT_CODE` from, so this reuses established wiring rather than introducing a new shared module. Hand-rolled column-width padding, no new dependency, matching this project's consistent preference (e.g. ADR-0012's hand-rolled frontmatter) over pulling in a table-formatting crate for this.

## Consequences

- The code-signing fix prevents *future* keychain breakage; it does not retroactively repair already-desynced entries — one more manual re-add should be expected immediately after adopting it, then it should stop recurring.
- All four CLI changes are breaking, pre-1.0 renames with no back-compat shim, consistent with this project's stated preference against compatibility hacks — any existing shell aliases or scripts using the old flag/command names need updating.
- `sync`'s default output landing under a temp directory when `--local-output` is omitted is a real data-loss risk if a user never overrides it and the OS clears `/tmp` — named explicitly here as an accepted risk of the requested default, not glossed over.

## Out of scope

- Any change to `email::credentials`/`remote::credentials` internals — confirmed not the bug's location.
- Linux/Windows keychain backend behavior — not evidenced as broken; the fix is scoped to macOS's code-signature-tied ACL model. ([#22](https://github.com/noisypigeon/pigeon/issues/22))
- Any other CLI renames/consolidations beyond the four listed.
- Implementation itself — like every ADR before it, this is a decision record only.
