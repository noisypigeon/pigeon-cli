# ADR-0011: scope scaleway/iam-policy access to specific Object Storage buckets

- **Author**: Willow Finch ([@noisypigeon](https://github.com/noisypigeon)).
- **Date**: 2026-09-25.
- **Status**: Accepted.

## Context

`scaleway/iam-policy` (ADR-0009, ADR-0010; currently `v1.1.0`) grants access purely at organization or project scope, via `scaleway_iam_policy` rule blocks (`organization_id`/`organization_permission_sets`, `project_ids`/`project_permission_sets`). There was no way to restrict an application to a specific, named set of Object Storage buckets while denying access to everything else in the project.

Before designing this, the actual Scaleway provider/API capabilities were checked directly against official docs, which ruled out several plausible-sounding approaches:
- `scaleway_iam_policy`'s `rule` block has no resource-name/bucket field at all — only `organization_id`, `project_ids`, `permission_set_names`, and an optional CEL `condition`.
- Scaleway does support resource-level `condition` expressions (e.g. `resource.name.startsWith("/folder/")`), but **Object Storage is explicitly excluded** from the list of products that support resource-level conditions today (only IAM, Key Manager, and Secret Manager are). So bucket-name matching cannot be done through `scaleway_iam_policy` at all, now or via any documented near-term mechanism.
- The actual bucket-scoped access mechanism is a separate resource, `scaleway_object_bucket_policy` — an S3-style JSON policy attached to **one specific bucket** via its `bucket` argument, naming a `Principal` (e.g. `application_id:<id>`) and granting `Action`s on `Resource` values scoped to that one bucket's name and object-key prefixes. There is no cross-bucket wildcard in `Resource` — each policy names exactly one bucket.
- No Scaleway data source enumerates or lists buckets by name/prefix (confirmed against the provider's `docs/data-sources` directory) — so a module cannot itself resolve "all buckets prefixed `data-`" into a concrete list; only a caller who already knows the candidate bucket names can filter them.
- Scaleway IAM's real default is zero permissions until a policy grants otherwise, and policy rules are pure allow-lists — there is no explicit deny. This means a blanket `ObjectStorageFullAccess`-style permission set granted via `organization_permission_sets`/`project_permission_sets` already grants access to **every** bucket in that scope, regardless of any bucket policy; the two mechanisms compose additively, and the broadest grant wins.

Given this, real bucket-level least-privilege has to be built on `scaleway_object_bucket_policy`, with the module resolving exact bucket names — wildcard/prefix matching is not a capability Scaleway (or this module) can provide, since the caller already possesses the literal bucket names from their own `scaleway/object-bucket` module calls and can filter them with plain Terraform (`startswith()`) before calling this module.

Separately, an unrelated in-flight change is folded into this same release: the existing `org_permission_sets` input is renamed to `organization_permission_sets`, for consistency with this module's other already-full-length names (`organization_id`, `project_permission_sets`, `project_ids`) rather than the one abbreviated holdout.

## Decision

### New inputs: `bucket_names` and `bucket_actions`

```hcl
variable "bucket_names" {
  type        = list(string)
  description = "Exact Object Storage bucket names to grant access to (no bucket access is granted by default)"
  default     = []
}

variable "bucket_actions" {
  type        = list(string)
  description = "S3 actions granted on each bucket in bucket_names (no actions are granted by default)"
  default     = []

  validation {
    condition = alltrue([
      for action in var.bucket_actions : contains([
        "s3:ListBucket",
        "s3:GetObject",
        "s3:PutObject",
        "s3:DeleteObject",
      ], action)
    ])
    error_message = "bucket_actions may only contain: s3:ListBucket, s3:GetObject, s3:PutObject, s3:DeleteObject."
  }
}
```

`bucket_names` takes exact, literal bucket names only — no glob/prefix syntax. Neither `bucket_names` nor `bucket_actions` defaults to a populated value — a stack must explicitly list both which buckets and which actions it wants, reinforcing "no resource access by default" at both levels rather than granting a preset action bundle the moment any bucket name is supplied. `bucket_actions` applies uniformly to every bucket in `bucket_names`; there is no per-bucket action customization. The `validation` block restricts entries to a known-valid set of S3 actions (matching this module's existing `scaleway/object-bucket` precedent of validating enum-like string inputs) to catch typos rather than silently producing a bucket policy with a misspelled, no-op action.

### New resource: one `scaleway_object_bucket_policy` per bucket

```hcl
resource "scaleway_object_bucket_policy" "bucket_access" {
  for_each = toset(var.bucket_names)

  bucket = each.value
  policy = jsonencode({
    Version = "2023-04-17"
    Statement = [
      {
        Sid       = "IamPolicyBucketAccess"
        Effect    = "Allow"
        Principal = { SCW = "application_id:${scaleway_iam_application.application.id}" }
        Action    = var.bucket_actions
        Resource  = [each.value, "${each.value}/*"]
      }
    ]
  })
}
```

`bucket_names` defaulting to `[]` is what gives "no bucket access by default" — zero `scaleway_object_bucket_policy` resources are created unless the caller opts in, matching how `organization_permission_sets`/`project_permission_sets`/`project_ids` already default to `null` (no grant). No new provider is required — `scaleway_object_bucket_policy` ships in the same `scaleway/scaleway` provider already pinned in `versions.tf`.

### Wildcard support is caller-side, not module-side

"Grant access to all buckets prefixed `data-`" is implemented by the *caller*, not this module: since the caller already holds the literal bucket names (typically from their own `scaleway/object-bucket` module calls' `name` outputs), they filter with plain Terraform before calling this module, e.g. `bucket_names = [for b in local.known_bucket_names : b if startswith(b, "data-")]`. This module has no pattern-matching input and cannot auto-discover buckets, since Scaleway exposes no bucket-listing data source.

### Composition hazard with blanket ObjectStorage grants

Documented explicitly rather than prevented in code: if a consumer also grants an `ObjectStorage*`-family permission set via `organization_permission_sets`/`project_permission_sets` in the same policy, that grant already covers every bucket in the relevant scope. Because Scaleway rules are allow-only, `bucket_names` cannot narrow that back down — real per-bucket least privilege requires *not* also granting a blanket Object Storage permission set alongside it.

### Rename: `org_permission_sets` → `organization_permission_sets`

Folded into this release since it's already in progress and this release already requires a coordinated `pigeon-do` update. Purely a naming-consistency fix; no behavior change.

## Consequences

- Breaking release (`release:major`), driven by the `organization_permission_sets` rename — the bucket-access addition itself is purely additive (`bucket_names`/`bucket_actions` both default to a no-op state).
- `pigeon-do`'s consumer (`scaleway/fr-par/noisypigeon.com/management/terraform/iam/iam.tf`) needs its `org_permission_sets` argument renamed and its pinned `ref` bumped; it can optionally start passing `bucket_names` later if it wants scoped bucket access, but doesn't need to as part of this release.
- A consumer wanting genuine bucket-level least privilege must consciously avoid also granting a blanket `ObjectStorage*` permission set in the same policy call.

## Out of scope

- Any automatic prefix/wildcard resolution inside the module — Scaleway has no primitive to support it (no bucket-listing data source, no cross-bucket `Resource` wildcard, no resource-level IAM condition support for Object Storage).
- Per-bucket differing `bucket_actions` — one uniform action list applies to every entry in `bucket_names`.
- Preventing or warning about the composition hazard with blanket `ObjectStorage*` permission sets in code (e.g. via a `validation` block) — documented here instead; both mechanisms remain independently available.
- Wiring `bucket_names` into `pigeon-do`'s existing consumer — that consumer doesn't need bucket-scoped access today.
