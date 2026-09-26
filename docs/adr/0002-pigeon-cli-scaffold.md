# ADR-0002: `pigeon` CLI scaffold and tooling

- **Author**: Willow Finch ([@noisypigeon](https://github.com/noisypigeon)).
- **Date**: 2026-09-22.
- **Status**: Accepted.

## Context

ADR-0001 specifies the `pigeon email` interface (authenticate, sink, transform) but not how the CLI itself should be built, structured, or tooled. Before implementing any real email logic, the `pigeon` binary was scaffolded end-to-end: argument parsing, command dispatch, stub handlers, toolchain pinning, and tests. This ADR is a backfill, written immediately after that scaffolding work, to record the decisions made and why — since none of them were specified in ADR-0001.

## Decision

### Toolchain

- [Mise](https://mise.jdx.dev) manages the Rust toolchain via `.mise.toml`, pinning `rust = "1.92.0"`. `mise install` gives any contributor the exact same compiler.
- Mise tasks (`build`, `test`, `fmt`, `fmt-check`, `lint`, `ci`) wrap the equivalent `cargo` commands so there's one documented entry point per workflow. `mise run ci` runs `fmt-check` + `clippy --all-targets --all-features -- -D warnings` + `test` as a single local gate.

### Package and binary naming

- The Cargo package name is `pigeon`, not `pigeon-cli` — the repository directory keeps the `pigeon-cli` name, but the package name is what determines the compiled binary name. This means `cargo build` produces `pigeon` directly with no `[[bin]] name = "pigeon"` override needed.
- Edition `2024`, `rust-version = "1.92"`.

### CLI argument surface

- [`clap`](https://docs.rs/clap) v4, derive API, is the single source of truth for commands, arguments, and `--help` text, living entirely in `service/pigeon-cli/src/cli.rs`:
  - `Cli` (top-level) → `Commands::Email(EmailArgs)` → `EmailCommands::{Authenticate, ListIdentities, Sink, Transform}`.
  - Each variant/field carries a doc comment, which clap renders directly as `--help` output — so help text and parsing behavior can never drift from a separately hand-maintained description.
- The four `EmailCommands` variants and their arguments mirror ADR-0001's interface examples verbatim:

  ```
  $ pigeon email authenticate first.last@example.com --alias first-last
  $ pigeon email list-identities
  $ pigeon email sink first-last --directory /tmp/first
  $ pigeon email transform --input /tmp/first --output /tmp/second --normalize --mbox-to-markdown
  ```

- `--alias` on `authenticate` is optional, matching the ADR literally. There is no identity store yet to validate against, so nothing today depends on it being required; this can be tightened once real authentication logic lands.

### Command dispatch structure

- `service/pigeon-cli/src/commands/mod.rs` exposes `dispatch(Commands) -> i32`, matching on the top-level `Commands` enum and delegating into one module per command group (currently just `service/pigeon-cli/src/commands/email.rs`). Adding a new top-level command group later means adding a new module and match arm, without touching `email.rs`.
- Each leaf handler function already takes the fully-typed, parsed arguments for its subcommand (e.g. `sink(alias: String, directory: PathBuf)`), even though the stub bodies ignore them. Swapping in real logic later is a body-only change — no signature or dispatch restructuring.

### Stub behavior

- Every `email` subcommand is unimplemented. Each handler prints `Not Yet Implemented` to stdout and returns exit code `1`.
- `1` was chosen deliberately:
  - `0` would falsely report success for an operation that did nothing.
  - `2` is reserved by clap itself for its own usage/parse errors (missing required argument, invalid value, etc.).
  - Keeping stub failures at `1` keeps "your input was rejected" (`2`) distinguishable from "your input was accepted but the feature doesn't exist yet" (`1`).

### Testing approach

- `service/pigeon-cli/tests/cli.rs` uses [`assert_cmd`](https://docs.rs/assert_cmd) + [`predicates`](https://docs.rs/predicates) to black-box test the CLI by spawning the actual compiled `pigeon` binary (`Command::cargo_bin("pigeon")`), rather than calling library functions in-process.
- This exercises real clap-rendered `--help` output and real process exit codes, not just argument-parsing shape.
- Test cases reuse ADR-0001's example invocations verbatim, so a passing suite doubles as an executable check that the CLI matches the ADR's documented interface.

### Out of scope

No real IMAP connectivity, authentication, sink, transform, or taxonomy/frontmatter logic exists yet — this ADR covers only the scaffold. Each of those features should get its own ADR (or a documented amendment here) once implemented, particularly if implementation reveals the argument shapes or stub conventions above need to change.
