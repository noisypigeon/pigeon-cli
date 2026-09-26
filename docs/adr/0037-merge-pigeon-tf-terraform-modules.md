# ADR-0037: merge `pigeon-tf` into this repo as `terraform/modules/*`

- **Author**: Willow Finch ([@noisypigeon](https://github.com/noisypigeon)).
- **Date**: 2026-09-25.
- **Status**: Accepted.

## Context

`pigeon-tf` (github.com/noisypigeon/pigeon-tf) is a separate repo of
versioned, reusable Terraform modules (DigitalOcean, Scaleway), consumed by
`pigeon-do` today via a sibling git clone pinned to a tag. This ADR merges it
into `pigeon-cli`, continuing the monorepo direction ADR-0036 started
(`service/pigeon-cli/` for the Rust CLI, `.github/` for repo-meta tooling):
`pigeon-tf`'s modules become `terraform/modules/{digitalocean,scaleway}/*`,
its two GitHub Actions workflows and one Claude Code skill come across
unchanged in path, and its 12 ADRs join this repo's single `docs/adr/`
sequence, renumbered starting at 0038.

This is a full history merge, not a snapshot copy: `pigeon-tf`'s real commit
history, all 20 of its existing release tags (`v0.1.0`-`v0.1.3` plus
per-module tags such as `digitalocean/access-key/v0.1.0` and
`scaleway/iam-policy/v3.0.1`), and its live release automation all need to
survive and keep working, not just its current file contents.

Confirmed via read-only inspection of `pigeon-tf` (`gh api`,
`raw.githubusercontent.com`) before any edit:
- Its tree: `digitalocean/*` (6 modules: `access-key`,
  `standard-storage-bucket`, `cold-storage-bucket`, `project`, `droplet`,
  `block-storage-volume`), `scaleway/*` (3 modules: `project`,
  `object-bucket`, `iam-policy`), `docs/adr/0001`-`0012*.md`,
  `.claude/skills/release-pr/SKILL.md`,
  `.github/workflows/{module-docs,module-release}.yml`, root `README.md`,
  `CLAUDE.md`, and a Terraform-specific `.gitignore` (`.terraform/`,
  `*.tfstate*`, `.terraform.lock.hcl`, `.DS_Store`) whose content collides
  with this repo's own root `.gitignore` (`/target`, `.DS_Store`).
- 20 existing git tags, confirmed via `gh api repos/.../tags`.
- This repo currently has **no** `.github/workflows/` and **no**
  `.claude/skills/` — nothing to collide with on those two paths.

Three placement questions were resolved with the user before writing this
decision:
- `pigeon-tf`'s README (a module index table + versioning/consumption notes)
  becomes a new `terraform/README.md`, not a section bolted onto this repo's
  root `README.md` (which is crates.io-facing per ADR-0018 and unrelated to
  Terraform).
- `pigeon-tf`'s `CLAUDE.md` (a one-line product description plus a 2-bullet
  ADR index) does not survive as its own file — its content folds into this
  repo's existing root `CLAUDE.md`, which already treats `docs/adr/` as one
  shared, repo-wide sequence (per ADR-0036). No per-service `CLAUDE.md`
  precedent exists yet, and introducing one only for `terraform/` would be
  inconsistent with how `service/pigeon-cli/` has none of its own.
- The standalone `pigeon-tf` GitHub repo itself is left untouched by this
  work — archiving or retiring it is a separate decision for later.

## Decision

### Target layout

```
terraform/
  README.md                        (was pigeon-tf's root README.md)
  .gitignore                       (was pigeon-tf's root .gitignore)
  modules/
    digitalocean/{access-key,standard-storage-bucket,cold-storage-bucket,project,droplet,block-storage-volume}/
    scaleway/{project,object-bucket,iam-policy}/
docs/adr/0038-0049-*.md            (was pigeon-tf's docs/adr/0001-0012-*.md)
.github/workflows/{module-docs,module-release}.yml   (unchanged path)
.claude/skills/release-pr/SKILL.md                   (unchanged path)
```

`pigeon-tf`'s own `CLAUDE.md` is deleted after its content is folded into
this repo's root `CLAUDE.md` (see below).

### ADR renumbering map

| pigeon-tf # | this repo # | title |
|---|---|---|
| 0001 | 0038 | pigeon-tf repo scaffold |
| 0002 | 0039 | pigeon-tf release automation |
| 0003 | 0040 | storage-bucket module renames |
| 0004 | 0041 | port droplet module |
| 0005 | 0042 | add block-storage-volume module |
| 0006 | 0043 | add scaleway provider |
| 0007 | 0044 | add scaleway/object-bucket module |
| 0008 | 0045 | scaleway/object-bucket namespaced naming |
| 0009 | 0046 | add scaleway/iam-policy module |
| 0010 | 0047 | scaleway/iam-policy permission scoping |
| 0011 | 0048 | scaleway/iam-policy bucket access |
| 0012 | 0049 | scaleway/iam-policy optional scopes |

Filenames keep their existing slugs; only the numeric prefix changes. Each
migrated ADR gains a new **Origin** bullet in its header block recording its
original repo and number (e.g. "pigeon-tf ADR-0001"), and any place it
references another `pigeon-tf` ADR by number is rewritten to the new number
above — a bare "ADR-0002" would otherwise be ambiguous once merged, since
this repo already has its own ADR-0002. References already qualified as
`pigeon-do`'s ADRs (a third, still-external repo) are left untouched.

### History-preserving merge mechanism

`git filter-repo` rewrites paths (not prose content) on a throwaway clone of
`pigeon-tf` before merging, so all 20 tags land pre-rewritten onto their
correct historical commits, and the merge itself is a clean,
conflict-free `git merge --allow-unrelated-histories`:

1. Clone `pigeon-tf` into a scratch directory.
2. Run `git filter-repo` with explicit path-rename pairs: the root
   `.gitignore` → `terraform/.gitignore`, `README.md` → `terraform/README.md`,
   `CLAUDE.md` → `terraform/CLAUDE.md` (kept through the rewrite so its
   introducing commit still renders correctly, deleted in a later commit
   once folded into root `CLAUDE.md`), `digitalocean` →
   `terraform/modules/digitalocean`, `scaleway` →
   `terraform/modules/scaleway`, and each of the 12 `docs/adr/00NN-*.md`
   files renamed per the table above. `.github/workflows/*` and
   `.claude/skills/*` are left alone since their paths already match this
   repo's target location. `git filter-repo` preserves every tag, repointing
   it at the corresponding rewritten commit — this is what makes re-tagging
   possible without hand-reconstructing 20 tags.
3. Add the rewritten clone as a temporary remote in this repo, `git fetch
   --tags`, then `git merge --allow-unrelated-histories`. Every path was
   already rewritten to its final, non-colliding location, so this merge is
   conflict-free. Remove the temporary remote afterward.

### Historical-citation rewrite (ADRs only)

Beyond the Origin bullet and internal ADR-number renumbering, each migrated
ADR's prose is reviewed occurrence-by-occurrence (per ADR-0036's own
established precedent — not a blind find-and-replace) for two more things:
bare `digitalocean/<module>` / `scaleway/<module>` path citations become
`terraform/modules/digitalocean/<module>` / `terraform/modules/scaleway/<module>`,
and mentions of "root `README.md`" meaning `pigeon-tf`'s own become
`terraform/README.md`. Citations of `.github/workflows/*.yml` or
`.claude/skills/.../SKILL.md` are left as-is, since those paths didn't move.

### Live automation gets functional edits, not citation rewrites

Unlike the ADRs (a historical record), `.github/workflows/module-docs.yml`,
`.github/workflows/module-release.yml`, and
`.claude/skills/release-pr/SKILL.md` are live and still run going forward, so
they're actually retargeted:
- `module-docs.yml`'s `working-dir:` list gets every entry prefixed with
  `terraform/modules/`.
- `module-release.yml`'s module-discovery `find digitalocean scaleway
  -mindepth 2 -maxdepth 2 -name versions.tf` becomes `find
  terraform/modules/digitalocean terraform/modules/scaleway ...`. Everything
  downstream (tag naming) derives from this `$dir`, so **future** releases
  after this merge tag as `terraform/modules/digitalocean/<module>/vX.Y.Z` —
  a deliberate, disclosed change from the pre-merge tag shape. The 20
  existing tags are not renamed (see below).
- `release-pr`'s `SKILL.md` gets its path mentions prefixed the same way, and
  its frontmatter `description` broadened from "a pigeon-tf Terraform
  module" to this repo's Terraform modules generally, since it no longer
  lives in a repo named `pigeon-tf`.

### Re-tagging the releases

`git filter-repo` already preserves all 20 tags pointing at the rewritten
commits during the scratch-clone step — no tag is manually reconstructed,
they ride along with the fetch and merge. Tag **names** are kept exactly as
they were (`v0.1.0`, `digitalocean/access-key/v0.1.0`,
`scaleway/iam-policy/v3.0.1`, etc.) even though the module now lives at
`terraform/modules/digitalocean/access-key` — a tag is a pointer to a
historical commit, not a claim about the current tree layout, so this isn't
a rename. After this ADR's PR lands on `main`, the 20 imported tags are
pushed to `origin` as an explicit, separately-confirmed step, since it writes
many refs to the shared remote at once. GitHub Release *objects* for the
tags that had them are not recreated — the same notes already live verbatim
in each module's `CHANGELOG.md`, which moves over intact with its module.

## Consequences

- This repo gains its first GitHub-Actions-driven automation
  (`module-docs.yml`/`module-release.yml`), coexisting with ADR-0029's
  otherwise fully local/manual dev cycle (`mise run ci` + manual PR merge) —
  two different automation philosophies in one repo now, noted plainly here,
  not reconciled.
- `pigeon-do`'s existing consumption of `pigeon-tf` (sibling clone, tag
  checkout, `source = "${local.pigeon_tf_root}/digitalocean/..."` paths) is
  broken by this move. Flagged here, not fixed here — the same
  flag-but-defer pattern `pigeon-tf`'s own ADR-0003 (now ADR-0040) used for a
  `pigeon-do`-side consequence it caused.
- Going forward, new module releases tag as
  `terraform/modules/<provider>/<module>/vX.Y.Z`; the 20 pre-merge tags keep
  their old, shorter form. Both are valid, just from different eras of the
  same module history.
- The standalone `pigeon-tf` repo keeps existing, with history that now
  diverges from its merged copy here — an accepted loose end per the
  "leave it alone for now" decision above.

## Out of scope

- Fixing `pigeon-do`'s consumption paths.
- Archiving or deleting the standalone `pigeon-tf` repo.
- Recreating GitHub Release objects for historical tags.
- Any `terraform fmt`/`validate` integration into `mise run ci` — the two
  automation systems stay independent, matching `pigeon-tf`'s own ADR-0002
  (now ADR-0039) out-of-scope note.
- A per-service `CLAUDE.md` convention beyond what's already decided above.
