# ADR-0061: split noisypigeon.com's domain config into per-provider root.hcl files

- **Author**: Willow Finch ([@noisypigeon](https://github.com/noisypigeon)).
- **Date**: 2026-09-25.
- **Status**: Accepted.
- **Origin**: `pigeon-do` ADR-0009.

## Context

After ADR-0060's provider-first restructure, a real bug surfaced: `terragrunt plan` in `digitalocean/sfo3/noisypigeon.com/data/email` (after its state was migrated) failed with:

```
Error: Reference to undeclared local value
  on bucket.tf line 4, in module "data_email":
   4:   region  = local.sfo3_region
A local value with the name "sfo3_region" has not been declared.
```

**Root cause**: ADR-0060 folded `noisypigeon.com/env.tf`'s locals (`tor1_region`, `sfo3_region`, `management_project`, `data_project`, `data_import_project`) directly into `noisypigeon.com.hcl`'s Terragrunt-level `locals` block. That was wrong: a Terragrunt `locals` block is only usable *within that same `.hcl` file* — e.g. for interpolating strings inside a `generate` block's `contents` heredoc. It is **not** automatically exposed as `local.x` to actual Terraform code running in a leaf. The old `env.tf` mechanism worked because `generate "env"` literally copied `env.tf`'s file content — a real Terraform `locals {}` block — into each leaf as a `.tf` file. Folding the values into Terragrunt's own locals dropped that generation step, so nothing ever wrote a real `locals {}` block containing these names into any leaf. `cloudflare_ids` and `bucket_names` don't have this problem specifically because they *are* implemented as `generate` blocks emitting real `.tf` files — ADR-0060 applied that pattern inconsistently.

Separately, the user asked to move `noisypigeon.com.hcl` (and its `.env`) to `digitalocean/root.hcl`. Since `pigeon.dev`'s DigitalOcean side was decommissioned (its resources destroyed, its leaves removed from the repo — see below), each of `noisypigeon.com`'s three providers is now effectively single-domain: every leaf under `digitalocean/`, `cloudflare/`, or `scaleway/` belongs to `noisypigeon.com`. That makes Terragrunt's idiomatic nesting-based discovery (`find_in_parent_folders("root.hcl")`) viable again, avoiding the fragile hardcoded-filename `include "domain"` block ADR-0060 introduced.

## Decision

### Split `noisypigeon.com.hcl` into three per-provider `root.hcl` files

- **`digitalocean/root.hcl`**: secrets loading, `generate "bucket_names"`, a **new** `generate "do_projects"` block that emits a real `locals {}` `.tf` file containing `tor1_region`/`sfo3_region`/`management_project`/`data_project`/`data_import_project` — this is the actual bug fix, replacing Terragrunt-only locals with a proper generated file, exactly matching how `cloudflare_ids` already works. Also `generate "provider"` with only `digitalocean`, and `remote_state`.
- **`cloudflare/root.hcl`**: secrets loading, `generate "cloudflare_ids"`, `generate "provider"` with only `cloudflare`, `remote_state`.
- **`scaleway/root.hcl`**: secrets loading, `generate "provider"` with only `scaleway`, `remote_state` (unused today — the Scaleway leaf keeps its own local-backend override, unchanged).

Every one of `noisypigeon.com`'s 10 leaves changes from:
```hcl
include "domain" {
  path = find_in_parent_folders("noisypigeon.com.hcl")
}
```
to:
```hcl
include "root" {
  path = find_in_parent_folders("root.hcl")
}
```
— found idiomatically per-provider now, no hardcoded filename. `common.hcl` is unchanged, still included by every leaf (old and new) for `pigeon_tf_root`.

`pigeon.dev`'s two Cloudflare leaves are untouched — they still use `include "domain" { path = find_in_parent_folders("pigeon.dev.hcl") }`, since `pigeon.dev.hcl` isn't part of this split (it's the one remaining per-*domain* file, not per-provider — `pigeon.dev` and `noisypigeon.com` both have Cloudflare leaves, so a shared `cloudflare/root.hcl` can't be domain-specific enough for both; keeping `pigeon.dev.hcl` separate and explicit sidesteps that entirely). This also means creating `cloudflare/root.hcl` doesn't collide with anything: `pigeon.dev`'s leaves never call `find_in_parent_folders("root.hcl")`, so they never see it.

### Avoiding a second round of state-key churn

`noisypigeon.com.hcl` sat at the true repo root, so `path_relative_to_include()` for a leaf like `digitalocean/sfo3/noisypigeon.com/data/email` naturally included `digitalocean/` in its state key. Moving `digitalocean/root.hcl` to live *inside* `digitalocean/` would drop that segment — the exact issue ADR-0059 hit when `root.hcl` moved into `pigeon.dev/`. Since the user had already migrated that leaf's state once, each new file reuses ADR-0059's fix: hardcode the provider name back into the key (`key = "digitalocean/${path_relative_to_include()}/terraform.tfstate"`, similarly for `cloudflare/`/`scaleway/`). **Verified empirically**: `terragrunt plan` in the already-migrated `digitalocean/sfo3/noisypigeon.com/data/email` succeeded with no "backend configuration changed" error and no undeclared-local error — confirming both the key-prefix fix and the bug fix work, with zero re-migration needed.

### Shared `.env` file

`noisypigeon.com.env`/`.env.example` move to `digitalocean/.env`/`.env.example` — the one shared secrets file for all three of `noisypigeon.com`'s providers. `cloudflare/root.hcl` and `scaleway/root.hcl` read this same file via an explicit `"${get_repo_root()}/digitalocean/.env"` path (not `find_in_parent_folders`, since it isn't their own ancestor) rather than duplicating credentials into three separate files. `.gitignore`'s `*.env` pattern already covers the new path.

### `pigeon.dev`'s DigitalOcean remnants

Already removed by the user directly (real DigitalOcean resources confirmed destroyed first) — `pigeon.dev/` no longer exists in the repo. One thing that commit swept up by mistake: `pigeon.dev.hcl` itself, which is still needed by `pigeon.dev`'s two Cloudflare leaves (unrelated to the DigitalOcean decommissioning). Restored as part of this ADR, unchanged in content from ADR-0060.

## Consequences

- The region/project-locals bug is fixed for all of `noisypigeon.com`'s DigitalOcean leaves, not just the one that surfaced it.
- Every `noisypigeon.com` leaf's `terragrunt.hcl` is simpler again (generic `include "root"`, no hardcoded per-domain filename).
- `pigeon.dev.hcl` had been accidentally deleted alongside the (correctly) decommissioned `pigeon.dev/digitalocean/*` — restored here.
- Each provider's leaves now only see that provider's `required_providers`/`provider` block (e.g. `digitalocean/root.hcl` no longer generates unused `cloudflare`/`scaleway` provider blocks for DO leaves) — a small correctness improvement over ADR-0060's combined 3-provider block.

## Out of scope

- Any further changes to `pigeon.dev.hcl` or `pigeon.dev`'s Cloudflare leaves.
- Moving the Scaleway leaf onto the shared state backend.
- The pre-existing, unrelated `1 to add` (`digitalocean_project_resources.this`) surfaced while verifying `digitalocean/sfo3/noisypigeon.com/data/email` — not caused by this change, left for the user to review.
