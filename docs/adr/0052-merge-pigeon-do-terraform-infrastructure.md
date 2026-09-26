# ADR-0052: merge `pigeon-do` into this repo as `terraform/infrastructure/*`

- **Author**: Willow Finch ([@noisypigeon](https://github.com/noisypigeon)).
- **Date**: 2026-09-25.
- **Status**: Accepted.

## Context

`pigeon-do` (github.com/noisypigeon/pigeon-do) is a separate, public repo:
Terragrunt/Terraform IaC for real personal infrastructure (DigitalOcean,
Cloudflare, Scaleway) across two live domains/accounts (`pigeon.dev`,
`noisypigeon.com`). This ADR merges it into this repo, continuing the
monorepo direction ADR-0037 established for `pigeon-tf`: `pigeon-do`'s tree
becomes `terraform/infrastructure/`, a new sibling to `terraform/modules/`,
and its 12 ADRs join this repo's single `docs/adr/` sequence, renumbered
starting at 0053.

**Unlike ADR-0037, this ADR does not execute its own merge.** `pigeon-tf`
was reusable-module-only content; `pigeon-do` directly represents real,
running cloud infrastructure and credential-adjacent configuration. The user
is performing the actual git surgery and any live-infra-touching steps
manually. This ADR documents the intended scaffolding and merge decisions so
that manual execution has a complete, actionable record to follow — like
every ADR before it, this is a decision record only.

Researched read-only via `gh api` (the same technique ADR-0037 used for
`pigeon-tf`), confirmed directly rather than assumed:

- **Current tree**: provider-rooted, per `pigeon-do`'s own ADR-0009 —
  `cloudflare/{root.hcl, global/{noisypigeon.com,pigeon.dev}/...}`,
  `digitalocean/{root.hcl, env.tf, global/..., tor1/...}`,
  `scaleway/{root.hcl, fr-par/..., global/...}`, root `common.hcl`,
  `.mise.toml`, per-provider `.env.example` (real `.env` files git-ignored
  and never tracked), `README.md`, `CLAUDE.md`, `docs/adr/0001-0012-*.md`
  (12 ADRs). No `.github/workflows/`, no committed `.claude/` (its own
  `.gitignore` excludes `.claude` entirely) — nothing to retarget on that
  front, unlike `pigeon-tf`'s two workflows plus one Claude Code skill.
- **No git tags** (25 commits, zero tags) — simpler than `pigeon-tf`'s
  20-tag preservation requirement; no re-tagging step needed here.
- **Every leaf's module source already resolves through what is now
  `terraform/modules/`**: every real `module` block (~14 of them, across
  ~10 `.tf` files) uses
  `source = "git::https://github.com/noisypigeon/pigeon-tf.git//<provider>/<module>?ref=<tag>"`
  — e.g. `.../pigeon-tf.git//digitalocean/access-key?ref=digitalocean/access-key/v0.1.0`.
  This is the exact consequence ADR-0037 flagged and deferred ("`pigeon-do`'s
  existing consumption of `pigeon-tf`... is broken by this move. Flagged
  here, not fixed here"), now concretely resolvable: the repo URL becomes
  `pigeon` and the subpath gets a `terraform/modules/` prefix, with every
  `?ref=<tag>` name unchanged — ADR-0037 already preserved those exact tags
  on the rewritten commits.
- **`common.hcl`'s `pigeon_tf_root`/`PIGEON_TF_PATH`/`generate "pigeon_tf"`
  block is dead code**: no `.tf` file in `pigeon-do` references
  `local.pigeon_tf_root` outside `common.hcl` itself. Every real module call
  uses the direct `git::` source above instead — confirmed, not assumed.
- **`terraform/.gitignore`** (this repo, moved from `pigeon-tf` by
  ADR-0037) already has `.terraform/`, `*.tfstate*`, `.terraform.lock.hcl`,
  `.DS_Store`. `pigeon-do`'s own `.gitignore` is a superset adding
  `.terragrunt-cache/` and `*.env`, plus a `.claude` line that doesn't apply
  here (this repo already tracks `.claude/skills/`).
- **This repo's root `.mise.toml` has no Terraform/Terragrunt awareness
  today.** `pigeon-do`'s own `.mise.toml` pins `terraform = "1.16.3"`,
  `terragrunt = "1.1.6"`, sets `[env] TG_TF_PATH = "terraform"`, and defines
  `fmt`/`fmt-check`/`plan`/`apply` tasks wrapping
  `terragrunt hcl format`/`terraform fmt -recursive`/`terragrunt run --all -- plan|apply`.
- **`pigeon-do`'s root `README.md` is stale**: it describes an
  account-rooted layout (`pigeon.dev/`, `noisypigeon.com/` as top-level
  directories) already superseded by `pigeon-do`'s own ADR-0008/ADR-0009
  with the provider-rooted layout actually on disk today. Unlike
  `pigeon-tf`'s README (copied verbatim by ADR-0037 because it was
  accurate), this one needs a rewrite to match reality, not a verbatim
  carry-over.
- **`pigeon-do`'s `CLAUDE.md`** is a one-line description plus an
  incomplete, stale ADR index (lists only ADR-0001, missing ADR-0002
  through ADR-0012). Same no-per-service-`CLAUDE.md` treatment ADR-0037 gave
  `pigeon-tf`'s: folds into this repo's root `CLAUDE.md`, then its own file
  is deleted.
- **Extensive cross-references to `pigeon-tf` ADRs by original number**
  appear throughout `pigeon-do`'s own ADRs (e.g. its ADR-0005: "pigeon-tf
  ADR-0002: per-module versioning", "pigeon-tf ADR-0003: module renames";
  its ADR-0008 cites a `pigeon-tf` module tag directly). Since ADR-0037
  already renumbered `pigeon-tf`'s ADR-0002/0003 to this repo's
  ADR-0039/0040, these citations need rewriting to the *new* numbers once
  merged — the same occurrence-by-occurrence discipline ADR-0036/ADR-0037
  already established, not a blind find-and-replace. **Distinct and
  unaffected**: citations of `pigeon-cli ADR-0002/0004` (e.g. in
  `pigeon-do`'s ADR-0001) already use this repo's real, never-renumbered
  ADR-0002/0004 — those need no change.
- **`pigeon-do` repo itself stays untouched** (not archived) — same
  leave-it-alone treatment ADR-0037 gave `pigeon-tf`.

## Decision

### Target layout

```
terraform/
  README.md                        (existing, modules index — gains one
                                     pointer sentence to infrastructure/)
  modules/...                      (existing, ADR-0037)
  infrastructure/                  (NEW — was pigeon-do's tree)
    README.md                      (was pigeon-do's root README.md,
                                     REWRITTEN to match the actual
                                     provider-rooted layout, not copied
                                     verbatim)
    common.hcl                     (pigeon_tf_root/generate block REMOVED —
                                     confirmed dead)
    cloudflare/{root.hcl, global/{noisypigeon.com,pigeon.dev}/...}
    digitalocean/{root.hcl, env.tf, global/..., tor1/...}
    scaleway/{root.hcl, fr-par/..., global/...}
docs/adr/0053-0064-*.md            (was pigeon-do's docs/adr/0001-0012-*.md)
```

`pigeon-do`'s own `CLAUDE.md` is deleted after its content folds into this
repo's root `CLAUDE.md` — ADR-0037's exact precedent.

### ADR renumbering map

This ADR is 0052; `pigeon-do`'s 12 ADRs start immediately after it:

| pigeon-do # | this repo # |
|---|---|
| 0001 | 0053 |
| 0002 | 0054 |
| 0003 | 0055 |
| 0004 | 0056 |
| 0005 | 0057 |
| 0006 | 0058 |
| 0007 | 0059 |
| 0008 | 0060 |
| 0009 | 0061 |
| 0010 | 0062 |
| 0011 | 0063 |
| 0012 | 0064 |

Filenames keep their existing slugs; only the numeric prefix changes. Each
migrated ADR gains an **Origin** bullet in its header block (e.g.
"pigeon-do ADR-0001"), its own internal self-references renumbered per this
table, and every `pigeon-tf ADR-000N` citation rewritten to ADR-0037's
renumbering map (0038-0049) — occurrence-by-occurrence, per ADR-0036's
established precedent, not a blind find-and-replace. Citations of
`pigeon-cli ADR-000N` are left untouched; they already point at this repo's
real, never-renumbered ADRs.

### History-preserving merge mechanism

Same recipe ADR-0037 used for `pigeon-tf`, minus tag handling (`pigeon-do`
has none):

1. Clone `pigeon-do` into a scratch directory.
2. Run `git filter-repo` with explicit path-rename pairs: the repo root →
   `terraform/infrastructure/` (covering `cloudflare/`, `digitalocean/`,
   `scaleway/`, `common.hcl`, `.mise.toml`'s content is merged rather than
   path-renamed — see below), `README.md` → `terraform/infrastructure/README.md`,
   `CLAUDE.md` kept through the rewrite (so its introducing commit still
   renders correctly, deleted in a later commit once folded into root
   `CLAUDE.md`), and each of the 12 `docs/adr/00NN-*.md` files renamed per
   the table above.
3. Add the rewritten clone as a temporary remote in this repo, `git fetch`,
   then `git merge --allow-unrelated-histories`. Every path was already
   rewritten to its final, non-colliding location, so this merge is
   conflict-free. Remove the temporary remote afterward.

### Historical-citation rewrite (ADRs only)

Beyond the Origin bullet, internal renumbering, and the `pigeon-tf`
citation rewrite above, each migrated ADR's prose is reviewed
occurrence-by-occurrence for path citations: bare `digitalocean/<leaf>` /
`cloudflare/<leaf>` / `scaleway/<leaf>` mentions become
`terraform/infrastructure/digitalocean/<leaf>` (etc.), and mentions of
"root `README.md`" meaning `pigeon-do`'s own become
`terraform/infrastructure/README.md`.

### Module source rewrite (resolves ADR-0037's deferred consequence)

Every leaf's
`git::https://github.com/noisypigeon/pigeon-tf.git//<path>?ref=<tag>`
becomes
`git::https://github.com/noisypigeon/pigeon.git//terraform/modules/<path>?ref=<tag>`
— tag names unchanged. Mechanical, but touches ~14 `source =` lines across
~10 real `.tf` files; each affected leaf needs a `terragrunt init` to
confirm it still resolves post-merge.

### `common.hcl`

The dead `pigeon_tf_root`/`PIGEON_TF_PATH`/`generate "pigeon_tf"` block is
removed rather than repointed, since nothing in the tree resolves through
it (see Context).

### `.gitignore`

`terraform/.gitignore` gains `.terragrunt-cache/` and `*.env`, merged in
from `pigeon-do`'s own `.gitignore`. Its `.claude` line is dropped — this
repo already tracks `.claude/skills/release-pr/`, so ignoring `.claude`
here would be actively wrong.

### `.mise.toml`

Root `.mise.toml` gains `pigeon-do`'s tool pins and env override:

```toml
[tools]
terraform = "1.16.3"
terragrunt = "1.1.6"

[env]
TG_TF_PATH = "terraform"
```

plus new tasks scoped to `terraform/infrastructure/`, mirroring
`pigeon-do`'s own task shapes:

```toml
[tasks.infra-fmt]
dir = "terraform/infrastructure"
run = ["terragrunt hcl format", "terraform fmt -recursive"]

[tasks.infra-fmt-check]
dir = "terraform/infrastructure"
run = ["terragrunt hcl format --check", "terraform fmt -recursive -check"]

[tasks.infra-plan]
dir = "terraform/infrastructure"
run = "terragrunt run --all -- plan"

[tasks.infra-apply]
dir = "terraform/infrastructure"
run = "terragrunt run --all -- apply"
```

This is this repo's first Terraform/Terragrunt-aware `mise` integration —
coexisting with, not replacing, ADR-0037's separate GitHub-Actions-only
automation for `terraform/modules/`.

### Root `README.md` and `terraform/README.md`

Root `README.md`'s Structure list gains a `terraform/infrastructure/`
bullet alongside the existing `terraform/modules/` one. `terraform/README.md`
gains a pointer sentence noting that `terraform/infrastructure/` is where
these modules are actually consumed.

## Consequences

- `terraform/infrastructure/` becomes this repo's first *live*
  infrastructure content, categorically distinct from `terraform/modules/`'s
  reusable-template-only content — worth stating plainly so it doesn't read
  as an inconsistency later.
- Three coexisting automation philosophies in one repo now: ADR-0029's
  local, fully manual `mise run ci` dev cycle; ADR-0037's GitHub-Actions
  module release/docs automation; and this ADR's new `infra-*` mise tasks.
  Noted here, not reconciled.
- No real secrets move as part of this merge: `pigeon-do`'s `.env` files
  were never tracked in its git history, so there's nothing to carry over
  or scrub on that front — only `.env.example` templates move.
- `common.hcl`'s dead `pigeon_tf_root` mechanism is removed, not carried
  forward.
- The standalone `pigeon-do` repo keeps existing, with history that now
  diverges from its merged copy here — the same accepted loose end ADR-0037
  left for `pigeon-tf`.

## Out of scope

- Actually executing the merge (the `git filter-repo` run, the merge
  commit, the module-source rewrite, the ADR renumbering/citation rewrite,
  the README rewrite, the `.mise.toml`/`.gitignore` edits) — performed
  manually by the user, given this repo's content directly represents live
  infrastructure and credential-adjacent configuration. Not performed by
  this ADR or by an agent.
- Archiving or deleting the standalone `pigeon-do` repo.
- Any redesign of the module-consumption model — e.g. switching from
  pinned `git::` remote sources to same-repo relative paths now that both
  trees live in one working tree. The decision above is the minimal,
  mechanical update to the existing mechanism; a bigger redesign is
  explicitly not this ADR's call.
- Implementation itself — like every ADR before it, this is a decision
  record only.
