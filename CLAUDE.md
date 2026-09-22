# CLAUDE.md

`pigeon` is a Rust CLI (clap + Mise), currently a scaffold with no real email logic implemented yet.

## ADRs govern this project

Before making architectural or interface changes, read the ADRs in `docs/adr/` and keep new work consistent with their decisions. If a change would contradict an existing ADR, flag it rather than silently diverging — prefer writing a new ADR (or updating an existing one's Status) over undocumented drift.

- `docs/adr/0001-email-data-sink-transform-cli.md` — the `pigeon email` product interface: authentication, sink, transform, file naming/taxonomy.
- `docs/adr/0002-pigeon-cli-scaffold.md` — CLI scaffold and tooling: package/binary naming, clap argument structure, command dispatch, stub/exit-code conventions, testing approach.
- `docs/adr/0003-imap-connectivity-and-authentication.md` — IMAP crate choice, per-provider authentication (app/bridge passwords, not OAuth2), credential lifetime, local credential storage, and multi-provider selection.
- `docs/adr/0004-mise-run-task-for-cli.md` — the `mise run pigeon` task for running the built binary.

## Commands

- `mise run build` — build the `pigeon` binary.
- `mise run pigeon -- <args>` — run the `pigeon` binary, e.g. `mise run pigeon -- email list-identities`.
- `mise run test` — run the test suite.
- `mise run fmt` / `mise run fmt-check` — format / check formatting.
- `mise run lint` — clippy, warnings denied.
- `mise run ci` — fmt-check + lint + test, the full local gate.
