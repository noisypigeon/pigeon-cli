# CLAUDE.md

`pigeon` is a Rust CLI (clap + Mise), currently a scaffold with no real email logic implemented yet.

## ADRs govern this project

Before making architectural or interface changes, read the ADRs in `docs/adr/` and keep new work consistent with their decisions. If a change would contradict an existing ADR, flag it rather than silently diverging — prefer writing a new ADR (or updating an existing one's Status) over undocumented drift.

- `docs/adr/0001-email-data-sink-transform-cli.md` — the `pigeon email` product interface: authentication, sink, transform, file naming/taxonomy.
- `docs/adr/0002-pigeon-cli-scaffold.md` — CLI scaffold and tooling: package/binary naming, clap argument structure, command dispatch, stub/exit-code conventions, testing approach.
- `docs/adr/0003-imap-connectivity-and-authentication.md` — IMAP crate choice, per-provider authentication (app/bridge passwords, not OAuth2), credential lifetime, local credential storage, and multi-provider selection.
- `docs/adr/0004-mise-run-task-for-cli.md` — the `mise run pigeon` task for running the built binary.
- `docs/adr/0005-email-sink.md` — `pigeon email sink`: identity selection, connecting, read-only enforcement (`EXAMINE`/`BODY.PEEK[]`), `.eml`-per-message output layout, resume mechanics, progress reporting.
- `docs/adr/0006-email-transform.md` — `pigeon email transform`: EML parsing/HTML-to-Markdown crates, ADR-0001 naming-scheme reuse, flat folder structure, taxonomy cross-cuts via namespaced frontmatter tags, attachment organization.
- `docs/adr/0007-email-sync.md` — merges `sink`+`transform` into `pigeon email sync`: per-UID fetch/transform/verify/delete pipeline, `.processed` resume marker, `source:`→`uid:` frontmatter amendment, `--debug sink`/`--debug transform` for phase-level (non-destructive) access. Reverses ADR-0001's permanent-raw-archive default.
- `docs/adr/0008-src-module-layout.md` — groups all email-specific code under `src/email/` (cli/commands/logic together), keeping top-level `src/cli.rs`/`src/commands/mod.rs` as thin roll-ups, so future command groups get their own sibling folder.
- `docs/adr/0009-remote-storage.md` — `pigeon remote`: rclone-style S3-compatible storage via the `minio` crate, bucket-scoped remotes (`name:path` addressing), `configure`/`list-buckets`/`ls`/`lsd`/`copy`, secret key in the OS keychain per ADR-0003's precedent, `src/remote/` reusing ADR-0008's module shape.
- `docs/adr/0010-remote-storage-improvements.md` — amends ADR-0009 from first real use: `name`→`alias` rename, reordered/simplified `configure` prompts, drops the dead `region` field and the unreliable inline list-buckets step (replaced by a `bucket_exists` verify-before-save check), concise S3 error formatting, and new `list`/`edit`/`remove` commands for configured remotes.

## Commands

- `mise run build` — build the `pigeon` binary.
- `mise run pigeon -- <args>` — run the `pigeon` binary, e.g. `mise run pigeon -- email list-identities`.
- `mise run test` — run the test suite.
- `mise run fmt` / `mise run fmt-check` — format / check formatting.
- `mise run lint` — clippy, warnings denied.
- `mise run ci` — fmt-check + lint + test, the full local gate.
