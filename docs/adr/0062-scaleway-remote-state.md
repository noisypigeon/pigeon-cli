# ADR-0062: switch scaleway/* to a Scaleway-native remote state backend

- **Author**: Willow Finch ([@noisypigeon](https://github.com/noisypigeon)).
- **Date**: 2026-09-25.
- **Status**: Accepted.
- **Origin**: `pigeon-do` ADR-0010.

## Context

`scaleway/root.hcl`'s `remote_state` block was scaffolded by ADR-0061 to point at the **shared DigitalOcean Spaces bucket** — the same one `digitalocean/root.hcl` and `cloudflare/root.hcl` use. That was always a placeholder: the block's own comment says "not actually used by the current Scaleway leaf yet." Every `scaleway/*` leaf has instead run on a local backend override (`remote_state { backend = "local" }` in each leaf's own `terragrunt.hcl`) since ADR-0060 explicitly deferred remote state as "Part 1 of the Scaleway rollout."

Since then, two things were bootstrapped, following the same runbook ADR-0055 established for DigitalOcean (bring up the leaf on a temporary local-backend override, apply, capture the real created name, wire it into `.env`):

- `scaleway/fr-par/noisypigeon.com/management/terraform-state/` — a real, applied Scaleway Object Storage bucket, `management-diw0s1-terraform-state`, region `fr-par` (endpoint `https://s3.fr-par.scw.cloud`), versioning enabled.
- `scaleway/global/noisypigeon.com/management/terraform-deployer/` — a Scaleway IAM application/policy/key (outputs `scw_access_key`/`scw_secret_key`) scoped to `InstancesFullAccess`, `ObjectStorageFullAccess`, `VPCFullAccess` across the `noisypigeon.com` and `pigeon.dev` projects — the automation identity for Scaleway Terraform runs.

With a real bucket and a real automation key in place, `scaleway/*` no longer needs local state (`.tfstate` files that are gitignored, never committed, and live only on whichever machine ran `apply`) or a borrowed DigitalOcean bucket for a provider that otherwise has no DigitalOcean dependency.

## Decision

### Point `scaleway/root.hcl`'s `remote_state` at the Scaleway bucket

```hcl
remote_state {
  backend = "s3"

  config = {
    endpoint                    = "https://s3.fr-par.scw.cloud"
    region                      = "fr-par"
    bucket                      = lookup(local.secrets, "SCALEWAY_TERRAFORM_STATE_BUCKET_NAME", "")
    key                         = "scaleway/${path_relative_to_include()}/terraform.tfstate"
    skip_credentials_validation = true
    skip_metadata_api_check     = true
    skip_region_validation      = true
    skip_requesting_account_id  = true

    access_key = get_env("SCALEWAY_ACCESS_KEY", lookup(local.secrets, "SCALEWAY_ACCESS_KEY", ""))
    secret_key = get_env("SCALEWAY_SECRET_KEY", lookup(local.secrets, "SCALEWAY_SECRET_KEY", ""))
  }

  generate = {
    path      = "backend.tf"
    if_exists = "overwrite"
  }
}
```

`key` is unchanged from today's scaffold — it already carries the `scaleway/` prefix per ADR-0061's fix, so no leaf's state key moves as a result of this change alone. `bucket` reads a new var, `SCALEWAY_TERRAFORM_STATE_BUCKET_NAME`, rather than `DIGITALOCEAN_TERRAFORM_STATE_BUCKET`. `access_key`/`secret_key` reuse the existing `SCALEWAY_ACCESS_KEY`/`SCALEWAY_SECRET_KEY` vars already wired into this same file's `provider "scaleway"` block — the `terraform-deployer` IAM key is intended to populate those going forward, so one credential pair serves both provider auth and backend auth, mirroring how `digitalocean/root.hcl` reuses `DIGITALOCEAN_SPACES_ACCESS_ID`/`SECRET_KEY` for both. The block's comment, which currently says this backend is "the shared Spaces bucket (same one digitalocean/root.hcl uses)," gets rewritten to describe the Scaleway bucket instead.

### New env var

`SCALEWAY_TERRAFORM_STATE_BUCKET_NAME` is added to `scaleway/.env.example` (documented, blank). Its real value in `scaleway/.env` (gitignored) is `management-diw0s1-terraform-state`.

### Per-leaf migration runbook

For each `scaleway/*` leaf (`terraform-state`, `terraform-deployer`, `project`, and any future leaf): remove the leaf's local `remote_state` override block from its own `terragrunt.hcl` — it then falls back to the now-Scaleway-pointed root block — and run `terragrunt init -migrate-state` to move its local `.tfstate` into the new backend. This is the same migration cost ADR-0061 already documents for any state-key change. This ADR documents the runbook; it does not execute it (see Out of scope).

### Divergence from ADR-0061

`scaleway/*` stops sharing the DigitalOcean Spaces bucket that ADR-0061 pointed it at. `digitalocean/*` and `cloudflare/*` are unaffected and keep using that shared bucket exactly as before — only `scaleway/root.hcl`'s backend changes.

## Consequences

- `scaleway/.env` gains a new required var, `SCALEWAY_TERRAFORM_STATE_BUCKET_NAME`.
- `scaleway/.env`'s `DIGITALOCEAN_SPACES_ACCESS_ID`/`DIGITALOCEAN_SPACES_SECRET_KEY`/`DIGITALOCEAN_TERRAFORM_STATE_BUCKET` entries become unused once every `scaleway/*` leaf has migrated — left in place until migration is confirmed complete, removable in a follow-up.
- Every `scaleway/*` leaf needs its local-backend override removed and a one-time `terragrunt init -migrate-state` run; this isn't automated and must be done leaf-by-leaf.
- `terraform-deployer`'s IAM key becomes load-bearing for both resource management and backend state access — rotating it means updating `scaleway/.env`.

## Out of scope

- Migrating `digitalocean/*`/`cloudflare/*` off the shared DigitalOcean Spaces bucket — unaffected by this change.
- Narrowing `terraform-deployer`'s IAM grant to a bucket-scoped key, mirroring DigitalOcean's `is_bucket_scoped` least-privilege pattern from ADR-0055 — the current grant stays broader; a future ADR can revisit this.
- Actually running the per-leaf migration runbook (`terragrunt init -migrate-state`) — this ADR documents the steps; execution is a separate follow-up task.
