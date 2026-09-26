# ADR-0008: group `email` into `service/pigeon-cli/src/email/`

- **Author**: Willow Finch ([@noisypigeon](https://github.com/noisypigeon)).
- **Date**: 2026-09-22.
- **Status**: Accepted.

## Context

`service/pigeon-cli/src/` today is almost entirely email-specific despite not being organized that way. Of its twelve files, only `cli.rs` (the top-level `Cli`/`Commands` parser) and `commands/mod.rs` (top-level dispatch plus `FAILURE_EXIT_CODE`) are actually generic — `commands/email.rs`, `credentials.rs`, `identity.rs`, `imap_client.rs`, `provider.rs`, `sink.rs`, `sync.rs`, and `transform.rs` all sit as flat top-level files despite being entirely about email. The intent going forward is to extend `pigeon` with other top-level commands beyond `email`. This ADR restructures `service/pigeon-cli/src/` now, before a second command group arrives and makes the flat layout worse, so the shape scales cleanly.

## Decision

### Target tree

```
service/pigeon-cli/src/
  main.rs
  lib.rs                  -- pub mod cli; pub mod commands; pub mod email;
  cli.rs                  -- Cli, Commands { Email(email::cli::EmailArgs) } -- roll-up only
  commands/
    mod.rs                -- top-level dispatch(Commands), FAILURE_EXIT_CODE (unchanged role)
  email/
    mod.rs                -- pub mod cli; pub mod commands; pub mod credentials; ...
    cli.rs                -- EmailArgs, EmailCommands, DebugPhase (moved from top-level cli.rs)
    commands.rs            -- moved from commands/email.rs, unchanged content
    credentials.rs
    identity.rs
    imap_client.rs
    provider.rs
    sink.rs
    sync.rs
    transform.rs
```

Everything email-specific moves under `service/pigeon-cli/src/email/`; file contents are unchanged except import paths. `service/pigeon-cli/src/cli.rs` keeps only the root parser (`Cli`) and the top-level `Commands` enum naming each command *group* — it stops growing as commands are added, since each group defines its own argument surface under its own folder. `service/pigeon-cli/src/commands/mod.rs` keeps its existing role exactly: top-level dispatch plus the shared `FAILURE_EXIT_CODE` exit-code policy ADR-0002 established. ADR-0008 doesn't touch its responsibilities, only what it delegates into.

### What moves, and why the boundary is drawn where it is

- **Business-logic modules move, not just the dispatch file.** The goal is extensibility for future commands, so the unit of grouping is "everything one command group needs" — its CLI args, its dispatch, its business logic — not just its dispatch handler. A lighter version that moved only `commands/email.rs` under `email/` while leaving `EmailArgs`/`EmailCommands` in the top-level `cli.rs` would leave `cli.rs` growing indefinitely as more commands are added, which is exactly the problem being fixed.
- **`identity.rs`, `credentials.rs`, `provider.rs`, and `imap_client.rs` move too — they aren't held back as shared infrastructure.** In their current concrete form (`Identity`'s fields, `Provider`'s Gmail/Fastmail/iCloud/Proton/Custom variants, IMAP-specific session handling) none of these are generic, despite the generic-sounding names. If a future command needs its own authentication or credential storage, it will need its own shape anyway. Extracting a genuinely shared abstraction belongs in a future ADR once a second concrete consumer actually exists — not speculatively now.

### `FAILURE_EXIT_CODE` stays where it is

It remains in `service/pigeon-cli/src/commands/mod.rs`, referenced from `email/commands.rs` as `crate::commands::FAILURE_EXIT_CODE` instead of `super::FAILURE_EXIT_CODE`. It's already the one place that owns the project's exit-code convention (ADR-0002); moving one of its consumers is not a reason to relocate the policy itself.

### Module style: `commands/mod.rs`-in-folder, not the newer sibling-file style

Rust 2024 (this project's edition) supports `service/pigeon-cli/src/email.rs` + `service/pigeon-cli/src/email/*.rs` as an alternative to `service/pigeon-cli/src/email/mod.rs` + `service/pigeon-cli/src/email/*.rs`. The codebase already uses the latter (`service/pigeon-cli/src/commands/mod.rs`) — `service/pigeon-cli/src/email/mod.rs` matches that existing convention rather than introducing a second, differing style in the same tree.

### `service/pigeon-cli/tests/cli.rs` needs no changes

It's a black-box test that spawns the compiled binary (ADR-0002's testing approach) and is completely decoupled from internal module structure. That this refactor requires zero test changes is itself evidence it's purely structural — no CLI-visible behavior, flags, or output change anywhere.

### Relationship to ADR-0002

ADR-0002 already established "one module per command group" (`service/pigeon-cli/src/commands/mod.rs` "delegating into one module per command group (currently just `service/pigeon-cli/src/commands/email.rs`)"). This ADR doesn't contradict that; it extends the same idea one level further: a command group isn't just a dispatch module, it's a whole domain — args, dispatch, and logic together — so it gets a folder, not a file. ADR-0002's Status doesn't change; this ADR refines that one sentence for a scale ADR-0002 didn't anticipate: a second command group actually arriving.

## Consequences

- Adding a second command group later means: a new `service/pigeon-cli/src/<name>/` folder mirroring `email/`'s shape (`mod.rs`, `cli.rs`, `commands.rs`, its own logic modules), one new `Commands` variant in top-level `cli.rs`, and one new match arm in `commands/mod.rs`. None of `email/`'s files need to change for that to happen.
- Every internal `use` path referencing a moved module gets a mechanical `crate::email::`-prefixed update. No behavior changes anywhere — same commands, same flags, same output, same exit codes.
- `docs/adr/*.md`'s existing file-path references (e.g. ADR-0005/0006/0007 pointing at `service/pigeon-cli/src/sink.rs`, `service/pigeon-cli/src/transform.rs`, `service/pigeon-cli/src/commands/email.rs`) become stale pointers once implemented; not rewritten retroactively, since ADRs are a historical record of decisions as made, not living documentation — this ADR is what future readers cross-reference to know the paths moved.

## Out of scope

- Implementation itself — like every ADR before its own separate implementation request, this is a decision record only.
- Extracting any genuinely shared or generic auth/credential abstraction — deferred until a second concrete consumer exists.
- Any change to CLI-visible behavior, flags, or output.
