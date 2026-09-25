# Changelog

All notable changes to this module are documented in this file.

The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

## [1.0.0] - 2026-09-25

### Add namespaced naming to scaleway/object-bucket

Ports `digitalocean/standard-storage-bucket`'s `{namespace}-{random}-{name}` bucket naming scheme to `scaleway/object-bucket`, via the same `random_string` resource shape (6-character lowercase alphanumeric suffix).

**Breaking**: adds a new required `namespace` input, and changes `name`'s meaning from "the full bucket name" to "the bucket name suffix" — the bucket's actual name is now computed as `${namespace}-${random}-${name}`. The `name` output's description is updated to "Computed bucket name" to reflect this. No consumers reference this module yet, so there's no real migration needed today, but this follows this repo's convention of labeling input-schema changes as breaking regardless. `enable_versioning` and `storage_class` are unaffected. See ADR-0008 for the full reasoning.

[#14](https://github.com/noisypigeon/pigeon-tf/pull/14)

## [0.1.0] - 2026-09-25

### Add scaleway/object-bucket module

Adds `scaleway/object-bucket`, a simple wrapper around `scaleway_object_bucket` with a required `name`, an `enable_versioning` toggle, and a `storage_class` input (`standard`/`glacier`).

`scaleway_object_bucket` has no bucket-level `storage_class` argument — storage class only exists inside a `lifecycle_rule`'s `transition` block. `storage_class = "glacier"` emits a single lifecycle rule that transitions new objects to Glacier immediately (`days = 0`); `storage_class = "standard"` emits no lifecycle rule, leaving objects at the resource's own default. This is also the first `validation {}` block in `pigeon-tf`, enforcing `storage_class` is one of `standard`/`glacier`.

The module is intentionally minimal for now — `project_id`, `region`, `tags`, `acl`, `force_destroy`, and general `cors_rule`/`lifecycle_rule` passthrough are deferred to a future change. See ADR-0007 for the full set of decisions, including why this module keeps the `object-bucket` name despite ADR-0003's earlier rename of the DigitalOcean equivalent.

[#13](https://github.com/noisypigeon/pigeon-tf/pull/13)
