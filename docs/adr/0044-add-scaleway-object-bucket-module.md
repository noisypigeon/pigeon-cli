# ADR-0007: add scaleway/object-bucket module

- **Author**: Willow Finch ([@noisypigeon](https://github.com/noisypigeon)).
- **Date**: 2026-09-25.
- **Status**: Accepted.

## Context

`pigeon-tf`'s second Scaleway module (after `scaleway/project`, ADR-0006), wrapping `scaleway_object_bucket` with a required `name`, a versioning toggle, and a `storage_class` input accepting `standard` or `glacier`. Two design questions had to be resolved before this could be built.

**`storage_class` has no direct home on the resource.** `scaleway_object_bucket` has no bucket-level `storage_class` argument — storage class only exists inside a `lifecycle_rule`'s `transition` block, which moves objects to a class after N days. Resolved with the user: `storage_class = "glacier"` emits a single `lifecycle_rule` doing an immediate (`days = 0`) transition to `GLACIER`; `storage_class = "standard"` emits no `lifecycle_rule` at all (new objects simply stay Standard, the resource's own default).

**Naming tension with ADR-0003.** ADR-0003 renamed `digitalocean/object-bucket` → `standard-storage-bucket` specifically because "object-bucket" named the module after a Terraform implementation detail (resource vs. data source) rather than the consumer's actual choice between standard and Cold Storage. This module covers both the standard and glacier tiers in a single resource via `storage_class`, so there's no resource/data-source split to hide behind an implementation-detail name — the situation ADR-0003 was correcting doesn't apply here. Resolved with the user: keep `scaleway/object-bucket`, matching the resource name and Scaleway's own "Object Storage" product branding.

## Decision

### Module: `scaleway/object-bucket`

Simple/minimal scope for now:
- `name` — the resource's one required argument.
- `enable_versioning` (bool, default `false`) — matches `standard-storage-bucket`'s existing `enable_versioning` naming for the same feature, wired into a `versioning { enabled = ... }` block.
- `storage_class` (string, default `"standard"`) — see mechanics below.

`project_id`, `region`, `tags`, `acl`, `force_destroy`, `object_lock_enabled`, and general `cors_rule`/`lifecycle_rule` passthrough are explicitly deferred (see Out of scope) — the resource's own optional arguments default sensibly (provider's project/region) without the module needing to expose them yet.

### `storage_class` mechanics

A `dynamic "lifecycle_rule"` block, present only when `storage_class == "glacier"`, containing one `transition { days = 0, storage_class = "GLACIER" }`. When `storage_class == "standard"`, no `lifecycle_rule` is emitted.

### Validation precedent

This is the first `validation {}` block anywhere in `pigeon-tf` — every existing enum-like input (e.g. `standard-storage-bucket`'s `acl`) is a bare string with no enforcement. `storage_class` validates against `["standard", "glacier"]`. Worth calling out since other modules may adopt this pattern for their own enum-like inputs later.

### Naming

Keeps `object-bucket` rather than following `standard-storage-bucket`'s renamed convention — see the Context section above.

## Consequences

- Consumers get a working Glacier toggle without hand-writing lifecycle rules.
- The module intentionally doesn't cover the resource's full surface area yet — extending it (project/region/tags/etc.) is expected as real usage surfaces the need.

## Out of scope

- `project_id`, `region`, `tags`, `acl`, `force_destroy`, `object_lock_enabled` inputs.
- General-purpose `cors_rule`/`lifecycle_rule` passthrough.
- `ONEZONE_IA` storage class.
- A separate `scaleway_object_bucket_acl`/policy module.
