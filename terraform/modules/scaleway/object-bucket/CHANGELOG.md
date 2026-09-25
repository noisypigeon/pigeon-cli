# Changelog

All notable changes to this module are documented in this file.

The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

## [0.1.0] - 2026-09-25

### Add scaleway/object-bucket module

Adds `scaleway/object-bucket`, a simple wrapper around `scaleway_object_bucket` with a required `name`, an `enable_versioning` toggle, and a `storage_class` input (`standard`/`glacier`).

`scaleway_object_bucket` has no bucket-level `storage_class` argument — storage class only exists inside a `lifecycle_rule`'s `transition` block. `storage_class = "glacier"` emits a single lifecycle rule that transitions new objects to Glacier immediately (`days = 0`); `storage_class = "standard"` emits no lifecycle rule, leaving objects at the resource's own default. This is also the first `validation {}` block in `pigeon-tf`, enforcing `storage_class` is one of `standard`/`glacier`.

The module is intentionally minimal for now — `project_id`, `region`, `tags`, `acl`, `force_destroy`, and general `cors_rule`/`lifecycle_rule` passthrough are deferred to a future change. See ADR-0007 for the full set of decisions, including why this module keeps the `object-bucket` name despite ADR-0003's earlier rename of the DigitalOcean equivalent.

[#13](https://github.com/noisypigeon/pigeon-tf/pull/13)
