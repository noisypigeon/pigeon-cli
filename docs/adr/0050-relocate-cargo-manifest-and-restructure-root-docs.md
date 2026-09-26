# ADR-0050: relocate Cargo manifest, split changelogs three ways, rewrite root docs

- **Author**: Willow Finch ([@noisypigeon](https://github.com/noisypigeon)).
- **Date**: 2026-09-26.
- **Status**: Accepted.

## Context

The repo has grown into a real monorepo (`service/pigeon-cli/` for the Rust
CLI, `terraform/modules/` for Terraform modules, `docs/adr/` shared across
both), but three root-level files still assume the old single-project
shape: `Cargo.toml`/`Cargo.lock` sit at repo root even though the crate's
own source has lived under `service/pigeon-cli/` since ADR-0036; there is
only one changelog (`service/pigeon-cli/CHANGELOG.md`) plus per-module
Terraform changelogs, with nothing giving a single glance across the whole
repo; and root `README.md`/`LICENSE` still read like a single-crate repo.

Confirmed directly (read-only) before planning the fix:
- `cargo package --list` run from repo root **currently bundles the entire
  repo** into the `pigeon-cli` crates.io package — every `docs/adr/*.md`
  (49 files), all of `terraform/`, `.github/`, `.claude/`, `CLAUDE.md`, not
  just the Rust source. This is real, confirmed packaging bloat/scope creep
  caused by `Cargo.toml` still living at repo root, not a hypothetical
  concern. ADR-0018's "No exclude/include list needed" call was correct
  when this was a single-crate repo; it no longer holds now.
- `service/pigeon-cli/tests/cli.rs` imports `pigeon::cli::Cli` (the lib
  crate is explicitly named `pigeon`, decoupled from the package name
  `pigeon-cli`), so the existing `[lib] name = "pigeon"` override in
  `Cargo.toml` must stay — only its `path` (and the `[[bin]]`/`[[test]]`
  paths) need to become relative to the manifest's new location.
- Root `README.md`'s `[LICENSE](LICENSE)` link and `Cargo.toml`'s
  `readme = "README.md"` field are the only two live places referencing
  these files by their current name/location; `docs/adr/0018`, `0036`,
  `0038` mention `LICENSE` too but only as historical, point-in-time prose
  (per this repo's established convention, left untouched).
- Confirmed with the user: the new root `/CHANGELOG.md` logs every PR
  across the whole repo (service + terraform), one line each, in addition
  to — not instead of — each PR's existing package-scoped changelog entry.

## Decision

### 1. Relocate `Cargo.toml`/`Cargo.lock` into `service/pigeon-cli/`

`git mv Cargo.toml service/pigeon-cli/Cargo.toml`,
`git mv Cargo.lock service/pigeon-cli/Cargo.lock`. Path fields inside
`Cargo.toml` become relative to the new manifest location:
`[lib] path = "src/lib.rs"`, `[[bin]] path = "src/main.rs"`,
`[[test]] path = "tests/cli.rs"` (all currently prefixed
`service/pigeon-cli/`). `readme = "README.md"` is left as the literal
string `"README.md"` — it now resolves to the new
`service/pigeon-cli/README.md` (created below) instead of the repo-root
one, which is exactly the desired crates.io-facing readme.

**mise stays the entry point** — every `mise run <task>` command keeps its
exact current name and usage; only the `cargo`/`cargo publish` invocations
inside `.mise.toml` gain `--manifest-path service/pigeon-cli/Cargo.toml`
(`build`, `pigeon`, `test`, `fmt`, `fmt-check`, `lint`, `publish-dry-run`,
`publish`). `mise run adr-issue` is untouched (no Cargo involvement).

**Build output moves with the manifest**: Cargo's default `target-dir`
follows the manifest's directory, so `target/` naturally becomes
`service/pigeon-cli/target/` once the manifest moves — no custom
`target-dir` override introduced. Both `.mise.toml`'s codesign step
(`build`/`pigeon` tasks) update from `target/debug/pigeon` to
`service/pigeon-cli/target/debug/pigeon`. A new
`service/pigeon-cli/.gitignore` gets `/target` (mirroring
`terraform/.gitignore`'s existing per-subtree pattern from ADR-0037); root
`.gitignore` drops its now-dead `/target` line, keeping only `.DS_Store`.

This supersedes ADR-0036's stated assumption that "`target/debug/pigeon`'s
build-output path is unaffected... since Cargo's `target/` dir follows the
workspace root, i.e. still repo root" — true when written (Cargo.toml
wasn't moving then), no longer true now that it is. Noted here rather than
edited into ADR-0036 itself, consistent with this repo's normal
point-in-time-record treatment of ADRs (ADR-0036's own src/tests/scripts
rewrite was the deliberate exception, not the rule).

### 2. Three-tier changelog model

- `service/pigeon-cli/CHANGELOG.md` — unchanged in spirit, already scoped
  to the Rust crate. Gains one clarifying line at the top pointing to the
  other two tiers.
- `terraform/modules/<provider>/<module>/CHANGELOG.md` — unchanged, already
  scoped per-module. **Left untouched** (no added cross-reference line):
  `module-release.yml`'s entry-prepend logic hardcodes `head -n 5` against
  today's exact 5-line header, and adding a line there would require
  coordinating that offset — not worth the risk for a cosmetic addition.
- **New `/CHANGELOG.md`** — one line per PR, across both `service/` and
  `terraform/`, sectioned by date (`## YYYY-MM-DD`), newest section first,
  not semantically versioned. Entry format:
  `- [<scope>] <one-line summary> ([#N](PR URL))`, where `<scope>` is
  `pigeon-cli` for service PRs, `terraform/<provider>/<module>` for a
  Terraform module PR, or `repo` for cross-cutting/structural ADRs (like
  this one, ADR-0036, ADR-0037). Starts fresh at this ADR's landing date —
  no backfill, matching how `service/pigeon-cli/CHANGELOG.md` itself started
  fresh at ADR-0029 rather than backfilling ADR-0001–0028.

**Dev cycle amendment** (`CLAUDE.md`'s "Dev cycle" section, step 5): for
every PR, append **both** the existing package-scoped bullet *and* a
one-liner to root `/CHANGELOG.md`'s current-date section (creating that
date's `##` heading if today doesn't have one yet), in the same follow-up
commit.

**`module-release.yml` gains the same behavior for terraform PRs**: after
computing `$TODAY`/`$PR_TITLE`/`$PR_URL` (already done in the "Update
changelogs and compute versions" step), for each changed module also
append `- [terraform/<provider>/<module>] $PR_TITLE ([#$PR_NUMBER]($PR_URL))`
into root `/CHANGELOG.md` — inserting under an existing `## $TODAY` heading
if present (via `awk`, insert-after-match), else creating a new `## $TODAY`
section at the top of the entries. `git add CHANGELOG.md` alongside the
per-module changelog in the same automated commit.

### 3. Rename `LICENSE` → `LICENSE.md`

`git mv LICENSE LICENSE.md`. Only live reference is root `README.md`'s link
(fixed as part of its rewrite below); historical ADR mentions of bare
`LICENSE` are left as point-in-time prose, unchanged.

### 4. `service/pigeon-cli/README.md` (new) — crates.io-facing

Seeded with: title/description (reusing today's root README's opening
lines), install (`cargo install pigeon-cli`), and a **command list**
enumerating the full CLI surface read directly from
`service/pigeon-cli/src/commands/{keyring,job}/cli.rs`:
`pigeon keyring add [email|bucket|encryption-key]`, `modify [alias]`,
`delete <alias>`, `list`; `pigeon job run email-sync [flags]`,
`decrypt-files [flags]`; plus a pointer to `pigeon --help`/`pigeon <group>
--help` for full detail. License line links the **absolute** GitHub URL
(`https://github.com/noisypigeon/pigeon-cli/blob/main/LICENSE.md`), not a
relative path — this file only packages what's under `service/pigeon-cli/`,
so a relative `../../LICENSE.md` link would 404 wherever a crates.io/docs.rs
render doesn't rewrite it against the repo.

### 5. Root `/README.md` — repo orientation

Rewritten to describe the monorepo shape and mise as the single entry
point, replacing today's single-crate-flavored content:
- **Structure**: `service/pigeon-cli/` (the Rust CLI — link to its README),
  `terraform/modules/` (Terraform modules — link to `terraform/README.md`),
  `docs/adr/` (architecture decisions governing every change here).
- **mise commands**: `build`, `pigeon -- <args>`, `test`, `fmt`/
  `fmt-check`, `lint`, `ci` — the existing `.mise.toml` task list, described
  once here instead of only living in `CLAUDE.md`.
- **License**: relative `[LICENSE.md](LICENSE.md)` link (same directory,
  safe).
- Drops the old "Install"/"Usage" content specific to the crate — that now
  lives in `service/pigeon-cli/README.md`.

### 6. Amend ADR-0018

Short dated amendment note (matching ADR-0036's own amendment precedent):
`readme = "README.md"` now resolves to `service/pigeon-cli/README.md`, not
repo-root `README.md`; the "No exclude/include list needed" call is now
correct *because* packaging naturally scopes to `service/pigeon-cli/` after
this move, not despite it.

## Consequences

- `target/debug/pigeon` moves to `service/pigeon-cli/target/debug/pigeon` —
  anyone with that old path memorized/scripted outside `mise` needs to
  update it.
- Every future PR now writes to two changelogs (its package-scoped one +
  root), a small but permanent addition to the dev-cycle checklist.
- `cargo publish`'s packaged file set shrinks to just `service/pigeon-cli/`
  going forward — a real fix, not just tidiness, given the confirmed
  current bundling of the whole repo.

## Out of scope

- Backfilling root `/CHANGELOG.md` with pre-ADR-0050 history.
- Adding a cross-reference line to the per-module Terraform changelogs
  (would require touching `module-release.yml`'s hardcoded header-length
  assumption for no functional benefit).
- Any Cargo workspace conversion — still a single relocated package, per
  ADR-0036's own already-established stance.
