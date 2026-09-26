# ADR-0055: management project and terraform-state bucket

- **Author**: Willow Finch ([@noisypigeon](https://github.com/noisypigeon)).
- **Date**: 2026-09-22.
- **Status**: Accepted.
- **Origin**: `pigeon-do` ADR-0003.

## Context

ADR-0054 stood up `pigeon-tf` with `digitalocean/{project,object-bucket,access-key}`, and `root.hcl` already has a `pigeon_tf_root` local ready for leaves to reference them. `pigeon-do` still has no real leaf stacks — two empty placeholder directories mark where the first ones should go:

- `pigeon.dev/digitalocean/global/management/{project.tf,terragrunt.hcl}`.
- `pigeon.dev/digitalocean/tor1/management/terraform-state/{bucket.tf,access_key.tf,terragrunt.hcl}`.

`topology-v1` already did almost exactly this for this same domain, in `pigeon.dev/digitalocean/management/{projects.tf,state.tf,terragrunt.hcl}`:

```hcl
# projects.tf
module "management" {
  source      = local.source_do_project_module
  environment = local.production
  name        = local.management
  purpose     = "Operational / Developer tooling"
  is_default  = true
}

# state.tf
module "pigeon_dev_management" {
  source    = local.source_do_object_bucket_module
  namespace = "pigeon-dev"
  name      = "management"
}
module "pigeon_dev_management_project_association" { ... }  # resource-project-attachment
module "pigeon_dev_management_key" {
  source           = local.source_do_access_key_module
  name             = module.pigeon_dev_management.name
  permission       = "fullaccess"
  is_bucket_scoped = false
}
```

A real gap surfaced while designing this: `root.hcl`'s `remote_state` bucket resolves to `lookup(local.secrets, "DIGITALOCEAN_TERRAFORM_STATE_BUCKET", "")`, and that key is currently empty in `.env`. **No leaf can `init` against the S3 backend today** — including the terraform-state leaf, whose entire job is to create the bucket that backend needs. This ADR has to resolve that bootstrap ordering problem, not just describe the two leaves.

## Decision

### Management project

`pigeon.dev/digitalocean/global/management/`:

- `terragrunt.hcl` — the standard leaf pattern already used everywhere: `include "root" { path = find_in_parent_folders("root.hcl") }`, `terraform { source = get_terragrunt_dir() }`.
- `project.tf`:
  ```hcl
  module "management" {
    source      = "${local.pigeon_tf_root}/digitalocean/project"
    name        = "Management"
    environment = "Production"
    purpose     = "Operational / Developer tooling"
    is_default  = true
  }
  ```

### Terraform-state bucket + key

`pigeon.dev/digitalocean/tor1/management/terraform-state/`:

- `bucket.tf`:
  ```hcl
  module "terraform_state" {
    source    = "${local.pigeon_tf_root}/digitalocean/object-bucket"
    namespace = "pigeon-dev"
    name      = "terraform-state"
  }
  ```
  `name = "terraform-state"`, not `topology-v1`'s literal `"management"` — self-describing, matches the directory and the bucket's actual purpose (→ `pigeon-dev-<random>-terraform-state`). Region left at the module's default (`tor1`), matching the directory.
- `access_key.tf`:
  ```hcl
  module "terraform_state_key" {
    source     = "${local.pigeon_tf_root}/digitalocean/access-key"
    name       = module.terraform_state.name
    permission = "fullaccess"
  }
  ```
  `is_bucket_scoped` left at the module's default (`true`) — scoped to just this bucket, least-privilege. Deliberate deviation from `topology-v1`'s unscoped `false`: this key's whole purpose is terraform-state bucket access, not general Spaces administration.
- `outputs.tf` exposes `access_key`/`secret_key` (both `sensitive = true`, inherited from the module) — not commented out the way `topology-v1` did. `sensitive = true` already keeps them out of plan/apply logs, so hiding them behind comments buys nothing and just adds friction to actually reading them via `terragrunt output` later.
- This key is a standalone artifact for now — **not** wired into `root.hcl`'s own provider/backend credentials, which keep using a pre-existing, manually-created master Spaces key (see Prerequisite below).
- No project-association: `topology-v1` attached this bucket to the management project via `resource-project-attachment`, but that module wasn't ported in ADR-0054. Deferred (see Out of scope).

### Bootstrap procedure (resolves the remote-state ordering problem)

1. `terraform-state`'s `terragrunt.hcl` starts with a local `remote_state` override:
   ```hcl
   include "root" {
     path = find_in_parent_folders("root.hcl")
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

   terraform {
     source = get_terragrunt_dir()
   }
   ```
   Terragrunt's default shallow merge lets this leaf's own `remote_state` block fully replace the one inherited from `root.hcl`, for this unit only.
2. Apply once. Note the bucket's `name` output.
3. Set `DIGITALOCEAN_TERRAFORM_STATE_BUCKET` in `.env` to that name.
4. Remove the local `remote_state` override from this leaf's `terragrunt.hcl`, reverting it to inherit root's S3 backend like every other leaf.
5. Run `terragrunt init -migrate-state` in this directory to move its state into the newly-created bucket.
6. Only after this can the `management` project leaf (or any future leaf) be initialized at all.

This is a one-time runbook, not a permanent architectural exception — after step 5, `terraform-state` behaves like every other leaf.

### `.gitignore` fix

`pigeon-do`'s `.gitignore` currently excludes `.terragrunt-cache/`, `.terraform.lock.hcl`, `.claude`, `.DS_Store`, `.env` — but not `*.tfstate*` or `.terraform/`. Step 1 above is the first scenario where a local `.tfstate` file (containing the access key's sensitive outputs) could actually exist in this repo. Both patterns must be added before that step runs.

### Prerequisite (not created by this ADR)

A broad, account-level DigitalOcean Spaces access key must already exist in `.env` (`DIGITALOCEAN_SPACES_ACCESS_ID`/`DIGITALOCEAN_SPACES_SECRET_KEY`), created manually via the DO control panel — the `digitalocean` provider needs it to manage Spaces bucket/key resources via its S3-compatible API at all. Same implicit prerequisite `topology-v1` relied on; this ADR doesn't create or rotate that key.

## Consequences

- `pigeon-do` gets its first two real, applyable leaves.
- `DIGITALOCEAN_TERRAFORM_STATE_BUCKET` becomes populated, unblocking remote state for every future leaf.
- The terraform-state leaf's local-backend override is temporary, removed once migrated — no standing exception.

## Out of scope

- `resource-project-attachment` — not yet ported to `pigeon-tf`, so the terraform-state bucket isn't attached to the Management project yet.
- `jumpbox`/`datastores` projects from `topology-v1`.
- Any other regions or leaves.
