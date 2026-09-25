# ADR-0008: namespaced bucket naming for scaleway/object-bucket

- **Author**: Willow Finch ([@noisypigeon](https://github.com/noisypigeon)).
- **Date**: 2026-09-25.
- **Status**: Accepted.

## Context

`digitalocean/standard-storage-bucket` names buckets with a fixed scheme — `{namespace}-{random_code}-{name}` (e.g. `example-com-q82q17-sample`) — via a `random_string` resource composed with two variables, `namespace` and `name`. `scaleway/object-bucket` (ADR-0007) currently takes `name` as the literal, full bucket name with no randomization or namespacing. This ADR ports the `standard-storage-bucket` scheme to `scaleway/object-bucket`.

`scaleway/object-bucket` isn't wired into any consumer yet — it isn't referenced anywhere in `pigeon-do` — so this has no real blast radius today. It's still labeled `release:major` on the PR, consistent with how this repo treats input-schema changes (matching ADR-0003's precedent of labeling schema-changing changes as breaking regardless of whether a real consumer exists yet).

## Decision

- Add a `namespace` variable (bucket name prefix), with the same explanatory header comment `standard-storage-bucket` uses.
- Reinterpret the existing `name` variable as the bucket name suffix — same variable, new meaning, now matching `standard-storage-bucket`'s own `name` semantics exactly (this is the breaking part: `name` previously was the whole bucket name).
- Add a `random_string` resource identical to `standard-storage-bucket`'s (`length = 6`, lowercase alphanumeric only, no uppercase/special characters).
- Compose the bucket's `name` argument as `"${var.namespace}-${random_string.suffix.result}-${var.name}"`.
- Add `hashicorp/random ~> 3.0` to `versions.tf`, mirroring `standard-storage-bucket`'s own pin for the same resource.
- Update the `name` output's description to "Computed bucket name" (matching `standard-storage-bucket`'s exact output description), since it's no longer a straight passthrough of the `name` input.
- `enable_versioning` and `storage_class` (ADR-0007) are unaffected.

## Consequences

- Breaking change to the module's public interface: `namespace` is a new required input, and `name`'s meaning changes from "the bucket name" to "the bucket name suffix." Labeled `release:major`.
- No known consumers exist yet, so no real migration is needed today — this is the same "breaking in principle, no current impact" situation ADR-0006/0007's new modules were in before any consumer existed.

## Out of scope

- Everything ADR-0007 already deferred (`project_id`, `region`, `tags`, `acl`, `force_destroy`, `object_lock_enabled`, general `cors_rule`/`lifecycle_rule` passthrough) remains deferred.
- Any change to `storage_class` or `enable_versioning` behavior.
