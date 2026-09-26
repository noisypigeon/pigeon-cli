# ADR-0060: add the Scaleway provider, and move to a provider-first directory layout

- **Author**: Willow Finch ([@noisypigeon](https://github.com/noisypigeon)).
- **Date**: 2026-09-25.
- **Status**: Accepted.
- **Origin**: `pigeon-do` ADR-0008.

This ADR was amended after initial acceptance to add a second decision (the provider-first layout / shared state bucket, below) — the original Scaleway-provider decision is unchanged and kept as-is.

## Context

`pigeon-do` currently wires exactly two providers — DigitalOcean and Cloudflare — identically into both accounts' `root.hcl` files (`pigeon.dev/root.hcl`, `noisypigeon.com/root.hcl`), per ADR-0059's per-account isolation model. Every leaf already receives both providers' `generate "provider"` blocks regardless of whether that leaf actually uses both (e.g. a DigitalOcean-only leaf still gets a `provider "cloudflare"` block) — there's no per-leaf provider selection today, just one shared block per account.

The user wants to start adopting [Scaleway](https://www.scaleway.com/) as a third provider, beginning with a Scaleway **Project** resource under `noisypigeon.com`. That project already exists in the real Scaleway organization (created outside Terraform) — the user will import it themselves, the same pattern already established for the recent Cloudflare DNS record imports (ADR-0059's successors, not a new ADR). Importing is explicitly out of scope here.

Research confirmed the exact shape needed:
- The official `scaleway/scaleway` Terraform provider's `required_providers` source and current major version (`~> 2.0`), and its authentication arguments (`access_key`, `secret_key`, `organization_id`, `project_id`, `region`, `zone`).
- `noisypigeon/pigeon-tf` already has a `scaleway/project` module, tagged `scaleway/project/v0.1.0` (confirmed via the GitHub API) — a thin passthrough wrapper around `scaleway_account_project`, taking optional `name`/`description`/`organization_id`. Its own `versions.tf` pins `scaleway/scaleway ~> 2.0`, matching what `root.hcl` needs to declare. Omitting `organization_id` in the module call falls back to the provider's own default `organization_id` — which is why the provider block needs it wired.

## Decision

### Provider wiring — both accounts

`pigeon.dev/root.hcl` and `noisypigeon.com/root.hcl` (superseded by `pigeon.dev.hcl`/`noisypigeon.com.hcl` post-amendment — see Part 2; `pigeon.dev`'s `provider "scaleway"` block became moot there since `pigeon.dev.hcl` doesn't declare Scaleway at all) both get a new `required_providers` entry:

```hcl
scaleway = {
  source  = "scaleway/scaleway"
  version = "~> 2.0"
}
```

and a new provider block, following the exact same `get_env("KEY", lookup(local.secrets, "KEY", ""))` interpolation style as the existing `digitalocean`/`cloudflare` blocks:

```hcl
provider "scaleway" {
  access_key      = "${get_env("SCALEWAY_ACCESS_KEY", lookup(local.secrets, "SCALEWAY_ACCESS_KEY", ""))}"
  secret_key      = "${get_env("SCALEWAY_SECRET_KEY", lookup(local.secrets, "SCALEWAY_SECRET_KEY", ""))}"
  organization_id = "${get_env("SCALEWAY_ORGANIZATION_ID", lookup(local.secrets, "SCALEWAY_ORGANIZATION_ID", ""))}"
}
```

Applied identically to both files, each reading its own account's `local.secrets` — continuing ADR-0059's per-account credential isolation, so `pigeon.dev` and `noisypigeon.com` can each hold a different Scaleway organization's credentials (or leave them blank) independently. This is wired into both accounts even though only `noisypigeon.com` has a Scaleway leaf today, mirroring how both accounts already carry unused Cloudflare/DigitalOcean provider blocks before every leaf needs them.

Only `access_key`/`secret_key`/`organization_id` are wired. `region`/`zone` are left unset: the one resource in scope (`scaleway_account_project`) is organization-scoped, not project- or region-scoped, and Scaleway's own defaults (`fr-par`/`fr-par-1`) are fine until a region-specific resource actually needs pinning — consistent with this repo's existing pattern of adding a knob only when something needs it (e.g. ADR-0058's `BUCKET_NAME_` prefix, added only once buckets existed to name).

### `.env.example` — both accounts

Three new keys added to both `pigeon.dev/.env.example` and `noisypigeon.com/.env.example` (renamed to `pigeon.dev.env.example`/`noisypigeon.com.env.example` at the repo root post-amendment — see Part 2), matching the existing plain-list style (no comment header per key):

```
SCALEWAY_ACCESS_KEY=
SCALEWAY_SECRET_KEY=
SCALEWAY_ORGANIZATION_ID=
```

### New leaf: `scaleway/global/noisypigeon.com/project/`

(Path shown post-amendment — see Part 2 below for why it's not `noisypigeon.com/scaleway/global/project/`.)

`terragrunt.hcl` — the standard boilerplate every leaf has (post-amendment shape — see Part 2), plus a local `remote_state` override since this leaf is deliberately kept off the shared backend for now:

```hcl
include "common" {
  path = find_in_parent_folders("common.hcl")
}

include "domain" {
  path = find_in_parent_folders("noisypigeon.com.hcl")
}

terraform {
  source = get_terragrunt_dir()
}

remote_state {
  backend = "local"
  generate = {
    path      = "backend.tf"
    if_exists = "overwrite"
  }
  config = {
    path = "${get_terragrunt_dir()}/terraform.tfstate"
  }
}
```

`project.tf` — calls the pinned `pigeon-tf` module, mirroring the existing `digitalocean/project` leaves' module-call shape:

```hcl
module "noisypigeon" {
  source = "git::https://github.com/noisypigeon/pigeon-tf.git//scaleway/project?ref=scaleway/project/v0.1.0"
  name   = "noisypigeon.com"
}
```

`name` is a placeholder for the user to adjust to match the real, already-existing Scaleway project before importing it — this ADR only scaffolds the leaf, it doesn't perform the import.

## Part 2: provider-first directory layout + shared state bucket

### Context

Creating the Scaleway leaf under `noisypigeon.com/scaleway/global/project/` prompted a bigger structural change: instead of `<domain>/<provider>/<region>/<leaf>` (ADR-0059's layout), reorganize to `<provider>/<region>/<domain>/<leaf>` — e.g. `cloudflare/global/noisypigeon.com/fastmail`, `digitalocean/tor1/noisypigeon.com/data-import/backblaze-import`, `scaleway/global/noisypigeon.com/project`. Rationale: a provider-wide rollout (like adding Scaleway) now touches one subtree instead of a copy per domain.

Alongside this, `pigeon.dev`'s DigitalOcean side is being decommissioned separately by its owner (out of scope here — its four DigitalOcean leaves and the empty `digitalocean/fra1` stub are **untouched** by this restructure, still served by the unchanged `pigeon.dev/root.hcl`/`pigeon.dev/.env`/`pigeon.dev/env.tf`). Only `pigeon.dev`'s two Cloudflare leaves (`fastmail`, `zone`) move into the new layout. This conveniently sidesteps a real credential conflict that would otherwise exist: `pigeon.dev`'s own DigitalOcean Spaces credentials (for managing its own Spaces buckets) vs. the shared state backend's Spaces credentials (`noisypigeon.com`'s, once state is consolidated) are the same env var pair today — with no DigitalOcean resources left under `pigeon.dev`'s new structure, there's nothing to conflict with.

Terraform state is consolidated onto `noisypigeon.com`'s existing bucket — `pigeon.dev`'s (remaining, Cloudflare-only) leaves now use it as their backend too. The Scaleway leaf is deliberately kept **off** the shared backend for now (local state, see Part 1's leaf definition above) — moving it to the shared backend is a future step.

### Decision

**Why this doesn't need a new untested Terragrunt mechanism.** Two constraints were already discovered empirically while implementing ADR-0059: (1) nested `include`s don't work ("only one level of includes is allowed"), and (2) a `locals` block in one included file can't forward-reference a `local.x` defined only in a sibling include. `common.hcl`'s "included directly by every leaf" pattern already works around both — this change reuses exactly that shape rather than inventing something new.

**`common.hcl` is unchanged** — still included by every leaf, still just `pigeon_tf_root`.

**Per-domain `root.hcl`-in-a-directory is replaced by a per-domain file at the true repo root** — `noisypigeon.com.hcl` and `pigeon.dev.hcl` — included by every leaf under that domain via a new `include "domain"` block, the same "extra include block" shape `common.hcl` already proved out:

```hcl
include "common" {
  path = find_in_parent_folders("common.hcl")
}

include "domain" {
  path = find_in_parent_folders("noisypigeon.com.hcl") # or pigeon.dev.hcl, per leaf's domain
}

terraform {
  source = get_terragrunt_dir()
}
```

`noisypigeon.com.hcl` carries everything the old `noisypigeon.com/root.hcl` had (secrets loading, `cloudflare_ids`, `bucket_names`, the full 3-provider `generate "provider"` block, `remote_state`), **plus** `noisypigeon.com/env.tf`'s locals (`tor1_region`, `sfo3_region`, `management_project`, `data_project`, `data_import_project`) folded directly into the same `locals` block. The separate `env.tf` file plus its `generate "env"`/`env_ancestors` machinery (ADR-0053) is dropped entirely: it existed to let any directory between a leaf and the domain root inject its own `env.tf`, but nothing ever used more than one level, and there's no longer a single domain-root directory for it to sit in once domain is nested under provider/region rather than being the top-level ancestor.

`pigeon.dev.hcl` is much smaller — secrets loading, `cloudflare_ids`, and a `generate "provider"` block with **only** `cloudflare` (no `digitalocean`/`scaleway` — nothing under `pigeon.dev`'s new structure needs them). Its `remote_state` block is shaped identically to `noisypigeon.com.hcl`'s, pointing at the same bucket; `pigeon.dev.env`'s `DIGITALOCEAN_TERRAFORM_STATE_BUCKET`/`SPACES_ACCESS_ID`/`SPACES_SECRET_KEY` are set to `noisypigeon.com`'s real values (the user's job when creating that file — never generated or guessed).

**State keys need no manual domain prefix anymore.** ADR-0059 needed `key = "pigeon.dev/${path_relative_to_include()}/terraform.tfstate"` specifically because `root.hcl` had moved into the domain directory, making the domain name disappear from `path_relative_to_include()`. Now that the domain file lives at the true repo root and the domain name is a segment of every leaf's own path (e.g. `cloudflare/global/noisypigeon.com/fastmail`), `path_relative_to_include()` naturally includes it again — both domain files just use `key = "${path_relative_to_include()}/terraform.tfstate"`.

**`.env`/`.env.example` move to the repo root**, renamed to match their domain file (`noisypigeon.com.env`, `pigeon.dev.env`, and `.example` variants). `.gitignore`'s `.env` line becomes `*.env` to match (an `.env.example`-suffixed file is unaffected, since it doesn't end in `.env`). `pigeon.dev/.env`/`.env.example`/`env.tf`/`root.hcl` are untouched — still serving the untouched DigitalOcean leaves.

**Directory moves**: all 10 of `noisypigeon.com`'s leaves move (dropping the `noisypigeon.com/` prefix, gaining a `noisypigeon.com` segment after `<provider>/<region>`); only 2 of `pigeon.dev`'s 6 leaves move (`cloudflare/global/{fastmail,zone}`). `noisypigeon.com/`'s old top-level directory is removed entirely once empty.

### Consequences

- Scaleway becomes available repo-wide the same way DigitalOcean/Cloudflare are: wired into both domain files, actual credentials supplied per-domain, actual usage opt-in per-leaf.
- `pigeon.dev` carries Scaleway/DigitalOcean provider config with no leaf using either yet under its new structure — its old DigitalOcean leaves remain, untouched, pending deletion by their owner.
- The new `scaleway/global/noisypigeon.com/project` leaf can't be planned/applied until `SCALEWAY_*` keys are filled in with real credentials and `project.tf`'s `name` is corrected to match the real project, followed by an import (left to the user, following the same declarative `import {}` block pattern used for the Cloudflare DNS records).
- Every moved leaf's Terraform state key changes (the directory path changed, and the key mirrors it), even for `noisypigeon.com`'s leaves that stay in the same bucket — so all 11 moved-and-remote-stated leaves (everything except the local-backend Scaleway leaf) need `terraform init -migrate-state` (or `-reconfigure`) before they'll plan again. This was verified empirically: after the restructure, a real `terragrunt init`/`plan` in the (safe, local-backend, no-real-state-at-risk) Scaleway leaf succeeded end-to-end — proving the new `include "common"` + `include "domain"` mechanism resolves correctly — while the other 11 leaves were deliberately left un-initialized for the user to migrate themselves.

## Out of scope

- Actually importing the existing Scaleway project into this leaf's state — the user does this themselves.
- `SCALEWAY_REGION`/`SCALEWAY_ZONE` env vars or any region/zone pinning — add them if/when a region-scoped Scaleway resource is introduced.
- `SCALEWAY_PROJECT_ID` / project-scoped provider config — not needed until a project-scoped resource (compute, storage, etc.) is added within the Scaleway project this leaf manages.
- Any `pigeon-tf` Scaleway module beyond `scaleway/project` (e.g. compute, storage) — added if/when needed.
- Running the actual `terraform init -migrate-state` for the 11 moved leaves — the user does this themselves.
- Decommissioning `pigeon.dev`'s DigitalOcean leaves/state bucket — being handled separately by the user, not by this ADR.
- Moving the Scaleway leaf onto the shared state backend — deferred to a future part.
