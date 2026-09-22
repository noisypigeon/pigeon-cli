# CLAUDE.md

`pigeon` is a Rust CLI (clap + Mise), currently a scaffold with no real email logic implemented yet.

## ADRs govern this project

Before making architectural or interface changes, read the ADRs in `docs/adr/` and keep new work consistent with their decisions. If a change would contradict an existing ADR, flag it rather than silently diverging — prefer writing a new ADR (or updating an existing one's Status) over undocumented drift.

- `docs/adr/0001-email-data-sink-transform-cli.md` — the `pigeon email` product interface: authentication, sink, transform, file naming/taxonomy.
- `docs/adr/0002-pigeon-cli-scaffold.md` — CLI scaffold and tooling: package/binary naming, clap argument structure, command dispatch, stub/exit-code conventions, testing approach.

## Commands

- `mise run build` — build the `pigeon` binary.
- `mise run test` — run the test suite.
- `mise run fmt` / `mise run fmt-check` — format / check formatting.
- `mise run lint` — clippy, warnings denied.
- `mise run ci` — fmt-check + lint + test, the full local gate.
