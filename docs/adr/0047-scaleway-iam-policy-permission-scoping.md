# ADR-0047: split scaleway/iam-policy permission grants by scope

- **Author**: Willow Finch ([@noisypigeon](https://github.com/noisypigeon)).
- **Date**: 2026-09-25.
- **Status**: Accepted.
- **Origin**: pigeon-tf ADR-0010, merged into this repo by ADR-0037.

## Context

`scaleway/iam-policy` (ADR-0046, released as `v0.1.0`) took a single `permission_set_names` input applied to one `rule` block scoped by `project_ids`. In practice, Scaleway IAM permission sets aren't uniformly project-scoped — grants like `IAMManager`, `ProjectManager`, and `IAMApplicationManager` are organization-level and don't make sense restricted to a `project_ids` list, while grants like `InstancesFullAccess` or `ObjectStorageFullAccess` are meant to be scoped to specific projects. A single `rule`/`project_ids` pair can't represent both at once.

This module is no longer hypothetical: `pigeon-do`'s `scaleway/global/noisypigeon.com/management/terraform-deployer/` leaf already consumes it, pinned to `ref=scaleway/iam-policy/v0.1.0`, to mint the automation identity for Scaleway Terraform runs. That leaf's own module call has already been updated (uncommitted, in `pigeon-do`) to pass `org_permission_sets`/`project_permission_sets` in anticipation of this change — granting `ProjectManager`/`IAMManager`/`IAMApplicationManager` at the organization level and `InstancesFullAccess`/`ObjectStorageFullAccess`/`VPCFullAccess` scoped to two specific projects. Its outputs, however, still read the old `scw_access_key`/`scw_secret_key` names, and its `ref` is still pinned to `v0.1.0` — so `pigeon-do` is currently in a half-migrated state this release needs to unblock.

Separately, ADR-0046 deferred renaming `scw_access_key`/`scw_secret_key` to `access_key`/`secret_key` (matching `terraform/modules/digitalocean/access-key`'s unprefixed naming) specifically because there was no consumer yet to coordinate with. Since this release already requires a coordinated `pigeon-do` update for the permission-scoping change, the output rename is folded in now rather than deferred a second time.

## Decision

### Split `permission_set_names` into `org_permission_sets` and `project_permission_sets`

`scaleway_iam_policy`'s `rule` block supports scoping by either `organization_id` or `project_ids`. The module now declares two `rule` blocks unconditionally:
- One scoped by the new `organization_id` input, granting `org_permission_sets`.
- One scoped by the existing `project_ids` input, granting `project_permission_sets`.

All four of `organization_id`, `org_permission_sets`, `project_ids`, and `project_permission_sets` are optional (default `null`), matching this module's existing "omit what you don't need" style for scoping inputs. In practice, since both `rule` blocks are always created, a consumer needs to supply a non-empty `permission_set_names`-equivalent for whichever rule it cares about — leaving one entirely unset while the other is populated is untested and likely to fail at apply time rather than silently produce a partial policy (see Out of scope).

### Rename outputs: `scw_access_key`/`scw_secret_key` → `access_key`/`secret_key`

Matches `terraform/modules/digitalocean/access-key`'s unprefixed output naming, completing the rename ADR-0046 deferred.

### Required `pigeon-do` follow-up

`pigeon-do`'s `scaleway/global/noisypigeon.com/management/terraform-deployer/iam_policy.tf` needs two changes once this releases: bump `source`'s `?ref=scaleway/iam-policy/v0.1.0` to the new tag, and update its two `output` blocks' `value`s from `module.terraform_deployer.scw_access_key`/`scw_secret_key` to `module.terraform_deployer.access_key`/`secret_key`. Its `org_permission_sets`/`project_permission_sets` module arguments are already updated in `pigeon-do` ahead of this release; it does not currently pass `organization_id`, relying on the input's default.

## Consequences

- Breaking change to an already-released module: `permission_set_names` no longer exists, and both output names changed. Labeled `release:major`.
- Unlike ADR-0045's `object-bucket` change, this one has a real, already-pinned consumer (`pigeon-do`'s `terraform-deployer` leaf) that must be updated in lockstep, not a zero-blast-radius change.
- Consumers wanting only organization-scoped or only project-scoped grants still get two `rule` blocks created; the unused one needs a deliberate empty-but-valid value rather than being omitted (see Out of scope).

## Out of scope

- Conditionally omitting a `rule` block (e.g. via `dynamic "rule"`) when its corresponding permission-set list is empty or unset — the module currently always emits both blocks. Revisit if a consumer needs org-only or project-only grants.
- Adding a `validation` block to require at least one of `org_permission_sets`/`project_permission_sets` be set.
- Actually performing the `pigeon-do` `ref` bump and output-reference update — that's a follow-up change in `pigeon-do`, not executed by this ADR.
