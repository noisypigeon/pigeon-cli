# ADR-0057: adopt pigeon-tf's per-module rename/versioning format

- **Author**: Willow Finch ([@noisypigeon](https://github.com/noisypigeon)).
- **Date**: 2026-09-23.
- **Status**: Accepted.
- **Origin**: `pigeon-do` ADR-0005.

## Context

`pigeon-tf` has evolved substantially since `pigeon-do`'s ADR-0054 scaffolded it. It now has its own `docs/adr/`: ADR-0038 backfilled the original scaffold decision, ADR-0039 added full release automation, and ADR-0040 renamed the two bucket modules. Two things from those ADRs directly concern `pigeon-do`.

### ADR-0040 (originally pigeon-tf ADR-0003): module renames

`digitalocean/object-bucket` → `digitalocean/standard-storage-bucket`, `digitalocean/object-bucket-cold` → `digitalocean/cold-storage-bucket` (via `git mv`, no input/output schema changes beyond tightened descriptions). ADR-0040 explicitly found and documented that this breaks two `pigeon-do` leaves referencing the old paths, and explicitly deferred the `pigeon-do`-side fix: *"The `pigeon-do`-side fix (updating the `source =` lines) is explicitly deferred to a separate follow-up task, not done here."* This ADR is that follow-up.

- `pigeon.dev/digitalocean/tor1/management/terraform-state/bucket.tf` — `source = ".../digitalocean/object-bucket"`
- `pigeon.dev/digitalocean/tor1/rolodex/email/bucket/bucket.tf` — `source = ".../digitalocean/object-bucket-cold"`

Checked both modules' current `inputs.tf` against these two leaves' existing arguments: no other changes are needed. `standard-storage-bucket` now requires `region` and `project` — both already supplied in `terraform-state/bucket.tf`. `cold-storage-bucket` requires `name`/`region` with optional `project` — already supplied in `rolodex/email/bucket/bucket.tf`. Only the `source =` path itself is stale in each.

### ADR-0039 (originally pigeon-tf ADR-0002): per-module versioning

Versioning moved from whole-repo tags (`v0.1.0`–`v0.1.3`, now frozen/historical) to per-module tags (`digitalocean/<module>/vX.Y.Z`). ADR-0039 explicitly flagged, but did not resolve, the consequence for `pigeon-do`: *"Once new changes stop landing under whole-repo tags, there's no longer one tag that captures 'all modules as of now' — `pigeon-do`'s consumption model will need its own follow-up decision (e.g. tracking `main` directly... or pinning per module)."*

`pigeon-do`'s own ADR-0054 currently says *"clone as a sibling directory and `git checkout` the tag you want"* — a whole-repo-tag model that no longer has a natural target. Checked: the current sibling clone has no `PIGEON_TF_PATH` override and isn't pinned to any tag today — it already just sits on `main` in practice.

### Extension: opt-in pinning

The "track `main`" decision below was originally paired with "any per-module pinning scheme" as explicitly out of scope. That's revisited here: some leaves want the reproducibility of a specific `pigeon-tf` release rather than always floating on `main`, and `pigeon-tf`'s per-module tags (`digitalocean/<module>/vX.Y.Z`) already give something precise to pin to. The floating default is unchanged; pinning is added as an opt-in per-leaf escape hatch.

## Decision

### Module path updates

- `terraform-state/bucket.tf`'s `source` → `${local.pigeon_tf_root}/digitalocean/standard-storage-bucket`.
- `rolodex/email/bucket/bucket.tf`'s `source` → `${local.pigeon_tf_root}/digitalocean/cold-storage-bucket`.
- No argument changes in either leaf.

### Consumption model: track `main`, not a pinned tag

This supersedes the specific "git checkout the tag you want" instruction from `pigeon-do`'s ADR-0054. The sibling clone now tracks `pigeon-tf`'s `main` via `git pull`, since per-module tags no longer give a single whole-repo version to pin to. This formalizes what's already true in practice (no tag checked out, no `PIGEON_TF_PATH` override) and matches ADR-0039's own suggested resolution, rather than inventing a per-module pinning scheme — which would be a real complexity increase inconsistent with the single-`pigeon_tf_root`-local, run-locally-only model every prior ADR here has kept simple.

Everything else about ADR-0054's local-sync decision is unchanged: sibling directory, no submodule, no sync script, no lockfile.

### Version-drift tradeoff, reaffirmed

`main` now moves more often (per-PR via automation, not just per manual tag), making the drift-is-possible tradeoff `pigeon-do`'s ADR-0054 already accepted more pronounced. Still accepted, for the same reason: Terragrunt only runs locally today.

### Pinning a module to a specific version (opt-in)

A leaf that wants a reproducible, specific `pigeon-tf` version — rather than always floating on `main` — replaces its `source =` with a Terraform-native `git::` remote module source, `ref`-pinned to the exact per-module tag `pigeon-tf`'s release automation produces:

```hcl
# Floating (default):
source = "${local.pigeon_tf_root}/digitalocean/standard-storage-bucket"

# Pinned (opt-in):
source = "git::https://github.com/noisypigeon/pigeon-tf.git//digitalocean/standard-storage-bucket?ref=digitalocean/standard-storage-bucket/v0.1.0"
```

- This is a plain per-leaf string choice — no new `root.hcl` local or `generate` block is needed, since the pinned form is a static URL, not a secret or machine-specific path.
- Deliberate, scoped exception to ADR-0054's original "no go-getter/network module source" principle: only a leaf that explicitly opts into a `git::...?ref=...` source incurs a network fetch (during `terraform init`). The floating default above stays fully local, unchanged.
- A local `git worktree`-per-pin approach (keeping pinning network-free too) was considered and not chosen, in favor of this simpler form with no extra bookkeeping.
- Un-pinning is symmetric: replace the `source =` back to the `${local.pigeon_tf_root}/...` form and re-`init`.
- Pinned and floating leaves are expected to coexist across the repo — pinning is a per-leaf decision, not a repo-wide mode.

### Extension: all current leaves pinned to v0.1.0

All 6 module usages across the 4 leaves that exist today (`terraform-state`'s `standard-storage-bucket`/`access-key`, `rolodex/email/bucket`'s `cold-storage-bucket`/`access-key`, and both `project` usages in `management`/`rolodex`) were switched from the floating `${local.pigeon_tf_root}/...` form to the pinned `git::...?ref=digitalocean/<module>/v0.1.0` form. At the time of this change, `v0.1.0` was confirmed as each module's latest tag and its content matched current `main` exactly, so this introduced zero drift.

This makes pinning the actual practice for every leaf today, not just an emergency escape hatch — reproducibility is wanted now, not only when something breaks. The mechanism itself is unchanged: `local.pigeon_tf_root`/the floating form is still available and still the documented default for any future leaf that doesn't need a pin.

**Consequence worth being explicit about**: a pinned leaf no longer picks up future `pigeon-tf` changes via `git pull` on the sibling clone. Bumping a pin is now a deliberate, visible `source =` edit per leaf, not an implicit side effect of keeping the local clone in sync.

## Consequences

- The two broken leaves work again on their next `plan`/`apply`.
- `pigeon-do` picks up every future `pigeon-tf` change automatically on the next local `git pull` — no per-module pin-tracking to build or maintain, for leaves that stay on the floating default.
- Reproducibility is looser than a pinned tag would give for floating leaves — an explicitly accepted cost, consistent with ADR-0054's original tradeoff.
- Pinned leaves incur a real network dependency at `init` time that floating leaves don't have.

## Out of scope

- Any automation to keep the sibling clone in sync — still a manual `git pull`.
- Migrating the floating/default consumption model off the sibling clone entirely (submodule, module registry).
- Automated pin bumping, or any registry/tracking of which leaves are currently pinned to what.
