# ADR-0049: make scaleway/iam-policy's scoping mechanisms independently optional

- **Author**: Willow Finch ([@noisypigeon](https://github.com/noisypigeon)).
- **Date**: 2026-09-26.
- **Status**: Accepted.
- **Origin**: pigeon-tf ADR-0012, merged into this repo by ADR-0037.

## Context

`scaleway/iam-policy` (`v2.0.0`) hit two related, confirmed problems surfaced by a real `pigeon-do` consumer, `data_email_iam` (`scaleway/fr-par/noisypigeon.com/data/email/iam.tf`):

**`for_each` crash.** `bucket_names` was a flat `list(string)`, fed directly into `bucket_access.tf`'s `for_each = toset(var.bucket_names)`. `data_email_iam` passes `bucket_names = [module.data_email.name]`, where `module.data_email.name` is `scaleway_object_bucket.bucket.name = "${namespace}-${random_string.suffix.result}-${name}"` — a value containing a `random_string` result unknown until apply on first create. `for_each` over a `toset()` uses each element as both the resource's addressing key and its value, so the key must be known at plan time; here it can't be. OpenTofu's own error message states the fix directly:

> it's better to use a map value where the keys are defined statically in your configuration and where only the values contain apply-time results.

**Unconditional `rule` blocks block bucket-only access.** `iam_policy.tf` always created both `rule` blocks (org-scoped and project-scoped) regardless of whether `organization_id`/`project_ids` were populated. Scaleway's `scaleway_iam_policy` requires each `rule` to set either `organization_id` or `project_ids` — so a consumer wanting *only* bucket-scoped access (leaving org/project fields at their `null` defaults) would have both rule blocks fail that requirement at apply time. This is exactly the gap ADR-0047 flagged and deferred:

> Conditionally omitting a `rule` block (e.g. via `dynamic "rule"`) when its corresponding permission-set list is empty or unset — the module currently always emits both blocks. Revisit if a consumer needs org-only or project-only grants.
>
> Adding a `validation` block to require at least one of `org_permission_sets`/`project_permission_sets` be set.

This ADR extends that to all three scoping mechanisms: at least one of (`organization_id` + `organization_permission_sets`), (`project_ids` + `project_permission_sets`), (`bucket_names` + `bucket_actions`) must be fully set, but none is individually mandatory.

The module's other live consumer, `terraform_deployer` (`scaleway/fr-par/noisypigeon.com/management/terraform/iam.tf`), fully populates both org and project pairs, sets no `bucket_names`, and is already applied to real infrastructure (per `pigeon-do`'s ADR-0010). Any fix here must not disrupt its existing state.

## Decision

### `bucket_names`: `list(string)` → `map(string)`

The caller now supplies a static, logical key per bucket (known at plan time) mapped to the bucket name (which may remain unknown until apply):

```hcl
variable "bucket_names" {
  type        = map(string)
  description = "Map of static logical key => exact Object Storage bucket name to grant access to (no bucket access is granted by default)"
  default     = {}
}
```

`bucket_access.tf`'s `for_each` now iterates the map directly (`for_each = var.bucket_names`, no `toset()`), with `bucket = each.value`. The resource's addressing key is the caller's static string, so only the *value* — the actual bucket name — is deferred to apply, which `for_each` allows. This is breaking, but since `bucket_names` has never successfully applied for any consumer, there's no real migration cost — `data_email_iam` just needs `bucket_names = [module.data_email.name]` rewritten as `bucket_names = { email = module.data_email.name }`.

### `scaleway_iam_policy.policy` becomes conditional; `rule` blocks become `dynamic`

```hcl
resource "scaleway_iam_policy" "policy" {
  count = (
    (var.organization_id != null && length(coalesce(var.organization_permission_sets, [])) > 0) ||
    (var.project_ids != null && length(coalesce(var.project_permission_sets, [])) > 0)
  ) ? 1 : 0
  ...
  dynamic "rule" {
    for_each = (var.organization_id != null && length(coalesce(var.organization_permission_sets, [])) > 0) ? [1] : []
    content { organization_id = var.organization_id; permission_set_names = var.organization_permission_sets }
  }

  dynamic "rule" {
    for_each = (var.project_ids != null && length(coalesce(var.project_permission_sets, [])) > 0) ? [1] : []
    content { project_ids = var.project_ids; permission_set_names = var.project_permission_sets }
  }
}
```

Each pair only produces a `rule` when it's *fully* populated (id/list set **and** the permission-set list non-empty) — superseding ADR-0047's untested assumption that a `rule` with a set `organization_id`/`project_ids` but an empty `permission_set_names` was an acceptable "deliberate empty-but-valid" no-op; instead, an incomplete pair now simply produces no rule at all. If neither pair is fully populated, the whole `scaleway_iam_policy` resource isn't created (`count = 0`) — a bucket-only application doesn't need an IAM policy at all, since `scaleway_object_bucket_policy` grants access independently by naming the application as `Principal`.

A `moved` block preserves `terraform_deployer`'s already-applied state across the new `count`:

```hcl
moved {
  from = scaleway_iam_policy.policy
  to   = scaleway_iam_policy.policy[0]
}
```

Without it, Terraform would plan to destroy and recreate a live, already-applied resource purely because of the addressing change.

### Cross-variable validation requiring at least one pair

```hcl
variable "name" {
  ...
  validation {
    condition = (
      (var.organization_id != null && length(coalesce(var.organization_permission_sets, [])) > 0) ||
      (var.project_ids != null && length(coalesce(var.project_permission_sets, [])) > 0) ||
      (length(var.bucket_names) > 0 && length(var.bucket_actions) > 0)
    )
    error_message = "At least one of organization_id+organization_permission_sets, project_ids+project_permission_sets, or bucket_names+bucket_actions must be fully set."
  }
}
```

Placed on `name` (the module's only unconditionally-required input) since Terraform/OpenTofu variable `validation` blocks may reference other variables only since language version 1.9. This mirrors the same gating logic used for the `count`/`dynamic` blocks above, so a configuration that would produce a useless, all-empty policy fails fast at plan time with a clear message instead of failing later against the Scaleway API (or, previously, not failing at all while silently doing nothing).

### Pin `required_version`

```hcl
terraform {
  required_version = ">= 1.9.0"
  ...
}
```

No `pigeon-tf` module currently pins a `required_version`. Since the cross-variable validation above depends on 1.9+ language support, this module now pins that floor explicitly rather than silently relying on it.

## Consequences

- Breaking release (`release:major`): `bucket_names`' type change, and a config that previously applied with all three scoping mechanisms empty will now fail validation.
- `data_email_iam` must change its `bucket_names` argument from a list to a map (never having successfully applied, this is free).
- `terraform_deployer` needs no call-site change beyond the version bump; the `moved` block protects its already-applied `scaleway_iam_policy.policy` state.
- Bucket-only configurations (no org/project grants at all) now actually work, closing the gap ADR-0047 deferred.

## Out of scope

- Further generalizing the "fully populated pair" concept beyond these three scoping mechanisms.
- A repo-wide `required_version` convention — this ADR pins it only on this module, consistent with ADR-0038's per-module self-containment.
