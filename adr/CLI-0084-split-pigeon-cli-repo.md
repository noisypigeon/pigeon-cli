# CLI-0084: split `service/pigeon-cli` into its own repository

- **Author**: Willow Finch ([@noisypigeon](https://github.com/noisypigeon)).
- **Date**: 2026-09-30.
- **Status**: Accepted.

## Context

`service/pigeon-cli` is splitting out of this monorepo into its own repo,
`noisypigeon/pigeon-cli` (already created; confirmed empty via `git
ls-remote` — nothing to lose in a force-push), for crates.io/public-facing
cleanliness: a dedicated repo reads better for a published Rust crate
(`cargo install pigeon-cli`) than a personal monorepo that also contains
terraform infrastructure and a blog.

This is a partial reversal of CLI-0036's "future monorepo" framing and of
the consolidation DEV-0037 and DEV-0052 did (merging the separate
`pigeon-tf` and `pigeon-do` repos' full histories *in*). Said explicitly
here rather than silently diverging — per this repo's own governance
practice (CLAUDE.md: "if a change would contradict an existing ADR, flag
it rather than silently diverging"). CLI-0036/DEV-0037/DEV-0052 are not edited;
they remain accurate records of what was decided and why, at the time.

**Mechanism**: unlike DEV-0037/0052 (which used `git filter-repo` to
rewrite and merge two independent histories together, including preserving
release tags across the rewrite), this split needs none of that
complexity — splitting apart doesn't have the tag/history-collision
problems merging two histories does. The chosen mechanism is: force-push a
full, byte-identical copy of this repo's current history to
`noisypigeon/pigeon-cli`, then land one ordinary new commit in *each* repo
that removes what doesn't belong there. Both repos keep their full,
unmodified git history going forward; nothing is rewritten. A side effect
worth naming: each repo's history will contain commits for content it no
longer has checked out (e.g. `pigeon-cli`'s `git log` will still show old
terraform-module commits) — accepted as the simple, low-risk tradeoff
against a `filter-repo` rewrite's added complexity and risk for a
one-directional split.

### What moves where

**`noisypigeon/noisypigeon`** (this repo) retains:
- `terraform/modules/`, `terraform/infrastructure/`
- `service/blog/`
- `docs/adr/*.md` → moved to `docs/archived/adr/*.md` (**all** existing
  ADRs, unconditionally — including pigeon-cli ones, kept for historical
  continuity, not deleted)
- `docs/report/*.md` → moved to `docs/archived/reports/*.md` (one file:
  `0001-email-sync-pull-transform-log-analysis.md` — ends up living only
  in `pigeon-cli` going forward since it's pigeon-cli-specific, but is
  archived here too per the same unconditional rule)
- `docs/USAGE.md`, trimmed to terraform+blog scope (the pigeon-cli and
  `mise`/cargo sections dropped)
- `CLAUDE.md`, pruned: drop every pigeon-cli ADR index line and the
  pigeon-cli-specific "Dev cycle (CLI-0029)" section (CLI-0029 has always
  governed `service/pigeon-cli` specifically — terraform module changes
  already follow their own process via the `release-pr` skill, confirmed
  by reading `docs/USAGE.md`'s own text); update the remaining
  (terraform-only) ADR index lines' paths to `docs/archived/adr/...`,
  their new true location
- `.mise.toml`, pruned to `terraform`/`terragrunt`/`zola` tools and tasks
  + the generic `adr-issue` task (kept in both repos) + `TG_TF_PATH`; the
  Rust/cargo tools and tasks are dropped
- `.gitignore`, pruned (drops `/target`, keeps `.DS_Store` and
  `/service/blog/public`)
- root `README.md` **untouched** — this is the GitHub profile README
  (the repo is named `noisypigeon/noisypigeon`, matching GitHub's
  profile-README convention; it reads as a personal bio, not a project
  description, and must not be treated as project documentation)
- `.github/workflows/` (`blog-pages.yml`, `module-docs.yml`,
  `module-release.yml`) and `.github/scripts/adr-issue.sh`

Removed entirely (not archived): `service/pigeon-cli/`, `docs/VISION.md`
(pigeon-cli-only per the product-walkthrough screenshots it contains).

**`noisypigeon/pigeon-cli`** (new repo) retains:
- `service/pigeon-cli/` (kept nested exactly as-is, not flattened to repo
  root — simplest, lowest-risk reading of "retain this path," easily
  revisited later as its own deliberate change if ever wanted)
- `docs/adr/`, filtered to the 51 pigeon-cli-related existing ADRs (list
  below) plus this ADR itself (52 total) — every other existing ADR is
  dropped here (still preserved, archived, in `noisypigeon`)
- `docs/report/` (the one file, kept live, not archived — nothing else to
  archive it *from* on this side)
- `docs/VISION.md`, kept as-is
- root `README.md`, newly written from `docs/USAGE.md`'s content, adapted
  to drop every terraform mention (this repo is free to have a normal
  project README, since it isn't a profile repo); `docs/USAGE.md` itself
  is deleted (its content now lives at the root, avoiding two competing
  "how to use this repo" docs)
- `CLAUDE.md`, pruned to pigeon-cli scope only (ADR index filtered to the
  52-file list, "Dev cycle" section kept as-is, terraform-specific prose
  dropped)
- `.mise.toml`, pruned to `rust` + the Rust/cargo tasks + `adr-issue`
- `.gitignore`, pruned (drops `/service/blog/public`, keeps `/target` and
  `.DS_Store`)
- `service/pigeon-cli/Cargo.toml`'s `repository` field corrected to
  `https://github.com/noisypigeon/pigeon-cli` (it was already stale —
  `https://github.com/noisypigeon/pigeon`, pre-dating even the
  `noisypigeon/noisypigeon` rename CLI-0051 documented)
- `.github/scripts/adr-issue.sh` (generic, kept in both repos)

Removed entirely: `terraform/`, `service/blog/`, `.github/workflows/`.

### ADR classification

Terraform/blog-only (archived-only in `noisypigeon`, never live in
`pigeon-cli`) — 32 files: **0037–0049**, **0052–0064** (the `pigeon-tf`/
`pigeon-do`-originated module and infrastructure ADRs), **0066**, **0069**,
**0070**, **0072** (Scaleway IAM/bucket fixes), **0067** (Zola blog
rewrite), **0079** (Scaleway compute-instance module).

Pigeon-cli-related (kept live in `pigeon-cli`, archived alongside
everything else in `noisypigeon`) — 51 existing files + this one: **0001–
0036**, **0050**, **0051**, **0065**, **0068**, **0071**, **0073–0078**,
**0080–0083**, **0084**.

Two files worth calling out individually:
- **CLI-0036** (monorepo-service-layout) stays pigeon-cli-live because it
  explains `service/pigeon-cli/`'s own internal `src`/`tests` layout,
  which persists unchanged — even though this split partially reverses
  its "future monorepo" framing.
- **CLI-0050** (relocate-cargo-manifest-and-restructure-root-docs) is
  genuinely dual-domain: it both relocated `Cargo.toml` into
  `service/pigeon-cli/` (still exactly how this repo is laid out) *and*
  established the now-obsolete monorepo-wide three-tier changelog/
  root-README convention. Kept pigeon-cli-live for the former reason.

### Other decisions

- Root `CHANGELOG.md` is kept in full, unmodified, in **both** repos going
  forward — not retroactively pruned by domain. Untangling which
  historical bullet belongs to which repo is unrequested, high-effort,
  low-value busywork; each repo's *future* entries naturally reflect only
  its own PRs from this point on.
- `service/pigeon-cli/CHANGELOG.md` moves with `service/pigeon-cli/`
  unchanged; no equivalent exists or is created in `noisypigeon`.
- Terraform module `CHANGELOG.md`s (nested under `terraform/modules/*/*/`)
  move with `terraform/` unchanged, no special handling.

## Decision

Execute per "What moves where" above: force-push this repo's current
`main` (full history) to `noisypigeon/pigeon-cli`, then land one ordinary
prune commit in each repo. `pigeon-cli`'s prune commit is this new repo's
first real commit (no PR infrastructure exists yet to route it through);
`noisypigeon`'s prune commit follows the normal CLI-0029 dev cycle it's
still governed by until that very commit retires the cycle's applicability
to this repo going forward (terraform changes already use the `release-pr`
skill instead, unaffected by this split).

## Consequences

- `noisypigeon/pigeon-cli`'s git history includes every terraform-module
  and blog commit this repo ever had, even though none of those files are
  checked out there — an accepted tradeoff of the simple full-copy
  mechanism (see Context).
- `noisypigeon`'s `docs/adr/` and `docs/report/` directories are empty
  immediately after this split (everything moved to `docs/archived/`) but
  remain the correct location for any *future* terraform-related ADR or
  report — nothing about the ADR/report process itself is being retired
  for this repo, only the historical pigeon-cli-authored content is being
  relocated.
- Going forward, `pigeon-cli` and `noisypigeon` are independent repos with
  independent issue trackers, PR history, and release cadence — a
  cross-cutting change that happens to touch both domains (rare, given how
  cleanly they've separated in practice) now needs two separate PRs in two
  separate repos instead of one.

## Out of scope

- Rewriting either repo's history to remove the other domain's commits/
  blobs entirely (a true `filter-repo` split) — the full-copy-then-prune
  mechanism was chosen specifically to avoid this complexity; revisiting
  it later (e.g. for repo size reasons) is a separate decision.
- Editing CLI-0036/DEV-0037/DEV-0052's own text to reflect this reversal — they
  remain accurate historical records; this ADR is the record of the
  reversal instead.
- A CI/GitHub Actions workflow for `pigeon-cli` — it had none before this
  split (`mise run ci` is local-only) and doesn't gain one as part of it.
