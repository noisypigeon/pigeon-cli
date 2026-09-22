# ADR-0004: `mise` task for running the CLI

- **Author**: Willow Finch ([@noisypigeon](https://github.com/noisypigeon)).
- **Date**: 2026-09-22.
- **Status**: Accepted.

## Context

ADR-0002 established `mise` as the single documented entry point per workflow: `build`, `test`, `fmt`, `fmt-check`, `lint`, and `ci` each wrap the equivalent `cargo` command. Missing from that list is the most basic workflow of all — actually running the built `pigeon` binary with arguments. Today there's no `mise` answer to that; a contributor has to fall back to `cargo run --` or invoking `target/debug/pigeon` directly, which breaks ADR-0002's own stated principle.

## Decision

- Add a `pigeon` task to `.mise.toml`:
  ```toml
  [tasks.pigeon]
  run = "cargo run --"
  ```
- The task is named `pigeon`, after the binary, not `run`. `mise run pigeon` reads as "run pigeon"; `mise run run` would not, and would leave it unclear what's being run.
- Arguments after `--` in the `mise run` invocation are appended by `mise` to the task's command, so `mise run pigeon -- email list-identities` executes `cargo run -- email list-identities`, which builds (if needed) and runs the binary as `pigeon email list-identities`. This mirrors `cargo run --`'s own argument-passthrough convention, just fronted by `mise`.

## Consequences

- `mise run pigeon -- <args>` is now the documented way to run the CLI, alongside the existing `build`/`test`/`fmt`/`lint`/`ci` tasks.
- `cargo run --`/direct binary invocation still work exactly as before; this ADR doesn't remove or change either, it just adds the missing `mise`-fronted path.
- No change to the CLI's own behavior, argument parsing, or exit codes.

## Out of scope

Any change to `pigeon`'s command surface, argument shapes, or runtime behavior — this ADR is tooling-only, covering how the binary is invoked during development, not what it does.
