# ADR-0086: move the crate from `service/pigeon-cli/` to repo root

- **Author**: Willow Finch ([@noisypigeon](https://github.com/noisypigeon)).
- **Date**: 2026-09-30.
- **Status**: Accepted.

## Context

The crate has lived at `service/pigeon-cli/` since ADR-0036, written when this repo was part of a larger monorepo also holding Terraform infrastructure and a blog — the nesting existed specifically to give each of those a sibling directory. ADR-0084 already split that monorepo apart: this repo now contains nothing but the pigeon-cli crate, `docs/adr/`, and repo tooling (`.mise.toml`, `.github/scripts/`, `CLAUDE.md`, `.claude/`). The nesting no longer serves any purpose, and the repo has carried several "two of everything" duplications since the split without yet consolidating them: two CHANGELOGs, two READMEs, two `.gitignore`s, and eight `mise` tasks that all thread a now-pointless `--manifest-path service/pigeon-cli/Cargo.toml` through a subdirectory that exists for no remaining reason.

Confirmed directly before planning:
- `service/` contains nothing but `pigeon-cli/` — moving its contents to root and deleting the empty `service/` directory is clean.
- Zero `.rs` source files anywhere in the repo contain `service/pigeon-cli` as a string literal — this is a pure directory/docs move with no code-correctness risk.
- Root `.gitignore` (`/target`, `.DS_Store`) is already a strict superset of the crate's own (`/target`) — no content merge needed.
- A repo-wide grep found 222 occurrences of the literal path `service/pigeon-cli/...` across 40 of the 53 ADR files in `docs/adr/`, plus 10 ADR-summary bullet lines in `CLAUDE.md`.
- **Packaging regression risk, confirmed via `cargo package --list`**: ADR-0050 moved `Cargo.toml` *into* `service/pigeon-cli/` specifically to fix real, confirmed packaging bloat — with the manifest at repo root, `cargo publish` bundled all 49 (now 53) ADR files, `.github/`, and `.claude/` into the crates.io tarball. Moving the manifest back to repo root without a package `include` list would silently reintroduce that exact bug, since `docs/adr/`, `CLAUDE.md`, and `.claude/` are all staying at root permanently (they didn't leave with ADR-0084 the way Terraform/blog did). This needs an explicit fix, not just a move.

**Confirmed with the user**: all 40 ADRs citing `service/pigeon-cli/...` get rewritten to the new root-relative path, occurrence by occurrence — matching this repo's own precedent (ADR-0036 and ADR-0051 both rewrote every prior ADR's path/link citations when the thing they pointed at moved, to keep `docs/adr/` an accurate "where is this now" map, not a frozen snapshot). The one carve-out: an occurrence describing a **historical fact** about a past state — e.g. ADR-0084's "splits `service/pigeon-cli` out of the former `noisypigeon/noisypigeon` monorepo into this repo" — stays exactly as written, since it's correctly describing what the directory was called at that moment in history, not a stale location pointer. Judged occurrence by occurrence, not blind find-and-replace (the same caveat ADR-0036 itself named).

Also requested: collapse the two `CHANGELOG.md`s into one (the crate's own, more detailed, Keep-a-Changelog-style one survives), merge the two `README.md`s into one (root survives, gains the crate's command reference), and merge the two `.gitignore`s (root's is already a superset). "Workflows" here means `.mise.toml`'s task definitions — this repo has no GitHub Actions workflows at all (`.github/` contains only `adr-issue.sh`).

## Decision

### 1. Move the crate to repo root

`git mv service/pigeon-cli/{Cargo.toml,Cargo.lock,src,tests} .`; delete the now-empty `service/` directory. `[lib]`/`[[bin]]`/`[[test]]` path fields in `Cargo.toml` (`src/lib.rs`, `src/main.rs`, `tests/cli.rs`) need no change — they're already relative to the manifest, which is exactly what makes this a pure move. `service/pigeon-cli/target/` is gitignored build output, not moved.

### 2. Scope crates.io packaging with an explicit `include` list

`Cargo.toml` gains:

```toml
[package]
# ...
include = ["src/**", "tests/**", "Cargo.toml", "Cargo.lock", "README.md", "LICENSE.md", "CHANGELOG.md"]
```

This is a **third amendment to ADR-0018** (following ADR-0050's and ADR-0051's own amendments to it): ADR-0050's "no exclude/include list needed" reasoning depended entirely on the manifest living in a subdirectory scoped to just the crate's files — true then, false again now that the manifest returns to a root that permanently also holds `docs/adr/`, `CLAUDE.md`, and `.claude/`. Verified via `cargo package --list --manifest-path Cargo.toml` post-move, pre- and post-`include`, mirroring exactly how ADR-0050 itself verified the opposite direction. `LICENSE.md` is added to the include list too — previously unreachable from `service/pigeon-cli/Cargo.toml` (Cargo can't include files outside the manifest's own directory), now naturally includable since the manifest and the license file share a root.

### 3. Collapse to one `CHANGELOG.md`

Delete root `/CHANGELOG.md` (ADR-0050's one-line-per-PR, date-sectioned, unversioned "repo-wide" tier — redundant once the repo holds only one crate). `git mv service/pigeon-cli/CHANGELOG.md CHANGELOG.md` — its existing versioned, Keep-a-Changelog-style log becomes the repo's only changelog. Historical entries already mentioning `service/pigeon-cli/...` stay as literal history, same principle as the ADR rewrites.

This **amends ADR-0029** (dev-cycle step 5 collapses from "append one bullet to the crate `CHANGELOG.md` and one to root `/CHANGELOG.md`" to a single bullet on the one surviving file) and **amends ADR-0050** (its three-tier changelog model collapses to one tier — the Terraform/blog tiers left with ADR-0084, the repo-wide tier merges here). `CLAUDE.md`'s own "Dev cycle" section is live documentation — updated to match directly, no historical exception.

### 4. Merge the two `README.md`s into one

Fold `service/pigeon-cli/README.md`'s `## Commands` section (the real command reference: `keyring add/modify/delete/list`, `job run email-sync`, `job run decrypt-files`) into root `README.md`, replacing today's two "see `service/pigeon-cli/README.md`" pointers. Delete `service/pigeon-cli/README.md`. Root `README.md`'s `## Structure` section drops its now-meaningless pointer at `service/pigeon-cli/` — the whole repo *is* the crate now. `Cargo.toml`'s `readme = "README.md"` needs no edit — once the manifest lives at root, that relative path already resolves to the merged README.

### 5. Merge `.gitignore`s

Root `.gitignore` already covers everything the crate's (`/target`) does — no content change. Delete `service/pigeon-cli/.gitignore`.

### 6. Update `.mise.toml`

Drop `--manifest-path service/pigeon-cli/Cargo.toml` from every task that has it (`build`, `pigeon`, `test`, `fmt`, `fmt-check`, `lint`, `publish-dry-run`, `publish`). `build`/`pigeon`'s codesign target and `exec` path change from `service/pigeon-cli/target/debug/pigeon` to `target/debug/pigeon`. `ci` and `adr-issue` are untouched.

### 7. Rewrite all 40 ADRs' path citations (222 occurrences)

One file at a time, `service/pigeon-cli/...` → the equivalent root-relative path, preserving any occurrence describing a historical fact rather than a current-location pointer. `CLAUDE.md`'s 10 affected ADR-summary bullet lines get the same treatment — live doc, always rewrite.

## Consequences

- `cargo build`/`cargo test`/etc. work directly from repo root with zero `--manifest-path` ceremony — matches how virtually every single-crate Rust repo is laid out.
- One changelog, one README, one `.gitignore` — removes duplication the repo has carried since ADR-0084 without yet consolidating.
- The `include` list is now the thing actually keeping `cargo publish`'s package scoped correctly, where previously directory placement did that job implicitly — a real, load-bearing addition, not cosmetic. If a future contributor adds a new top-level source directory without updating `include`, it silently won't ship; worth remembering, not a reason to avoid the fix.
- `target/debug/pigeon` moves from `service/pigeon-cli/target/debug/pigeon` back to `target/debug/pigeon` — anyone with the ADR-0050-era path memorized/scripted outside `mise` needs to update it.
- A materially large one-time documentation diff (~222 ADR line touches across 40 files, plus `CLAUDE.md`) to keep `docs/adr/` accurate — explicitly accepted by the user, matching established precedent (ADR-0036, ADR-0051).
- Amends ADR-0018 (a third time), ADR-0029, and ADR-0050 explicitly, per this repo's convention of stating amendments rather than silently diverging.

## Out of scope

- Any change to Rust source code itself — confirmed zero `service/pigeon-cli` string literals exist in `.rs` files.
- Adding real GitHub Actions workflows under `.github/workflows/` — none exist today; "workflows" here means only the existing `.mise.toml` tasks.
- Any change to `.github/scripts/adr-issue.sh` — already repo-root-relative, zero `service/pigeon-cli` references.
- Touching ADR *decision content* — only literal path citations change; substantive text, historical facts, and judgment calls in each ADR stay exactly as written.

Implementation lands in the same change as this ADR, per this project's established practice for structural moves (ADR-0036, ADR-0050, ADR-0084).
