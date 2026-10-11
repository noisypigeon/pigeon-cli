# ADR-0118: install `rustfmt`/`clippy` components explicitly in the release workflow

- **Author**: Willow Finch ([@noisypigeon](https://github.com/noisypigeon)).
- **Date**: 2026-10-10.
- **Status**: Proposed.

## Context

ADR-0117's merge triggered its own first real run of the new
`.github/workflows/release.yml` `release` job, and it failed immediately
at the `mise run ci` step:

```
[lint] error: 'cargo-clippy' is not installed for the toolchain '1.92.0-x86_64-unknown-linux-gnu'.
[lint] help: run `rustup component add --toolchain 1.92.0-x86_64-unknown-linux-gnu clippy` to install it
```

`jdx/mise-action` installs the Rust toolchain pinned by `.mise.toml`'s
`[tools] rust = "1.92.0"` via `rustup`, but a bare `rustup toolchain
install` only provisions `rustc`/`cargo` — not the `clippy` or `rustfmt`
components `mise run ci` (via `fmt-check`/`lint`) depends on. This never
surfaced before this ADR: `mise run ci` had only ever run on a developer's
own machine, where `rustup component add clippy rustfmt` was a one-time
step done long ago and simply never needed repeating.

This failed exactly as ADR-0117 designed it to: `cargo publish` never ran,
so `main` was never touched and no partial release state was left behind.
Re-running the job after this fix lands is a safe, ordinary retry.

## Decision

Add an explicit step to the `release` job in
`.github/workflows/release.yml`, between the `jdx/mise-action` setup step
and `mise run ci`:

```yaml
- name: Install rustfmt/clippy components
  run: rustup component add rustfmt clippy
```

The `build-binaries` job is unaffected — it only runs `mise run
build-release` (`cargo build --release`), which needs no components
beyond the base toolchain already installed by `mise-action`.

## Consequences

- The release workflow's `mise run ci` step now succeeds on a fresh
  runner instead of failing on every single run.
- No change to local developer workflow — components are already present
  there.

## Out of scope

- A broader audit of what else a fresh CI runner might be missing that a
  long-lived dev machine already has — this fix addresses the one gap
  actually hit in practice; further gaps (if any) get fixed when found.

## Verification

- `mise run ci` clean on the branch.
- Re-run the failed `release` workflow run on `main` (or wait for the next
  merge) and confirm `mise run ci` now passes the `lint`/`fmt-check` steps.
