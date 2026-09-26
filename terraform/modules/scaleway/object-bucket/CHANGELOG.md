# Changelog

All notable changes to this module are documented in this file.

The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

## [0.1.0] - 2026-09-26

### Consolidate as terraform/modules/scaleway/object-bucket 0.1.0

A Scaleway Object Storage bucket (`scaleway_object_bucket`) with a
namespaced, randomized name (`{namespace}-{random}-{name}`, matching
`standard-storage-bucket`'s scheme), a versioning toggle
(`enable_versioning`), and a `storage_class` input (`standard`/`glacier`,
implemented via an immediate `lifecycle_rule` transition when set to
`glacier`). See
[ADR-0044](../../../../docs/adr/0044-add-scaleway-object-bucket-module.md)
and
[ADR-0045](../../../../docs/adr/0045-scaleway-object-bucket-namespaced-naming.md)
for the full set of decisions behind this module's design.

Consolidates this module's prior `pigeon-tf` version history (`v0.1.0`,
`v1.0.0`) into a single 0.1.0 release as part of merging `pigeon-tf` into
this repo — see
[ADR-0037](../../../../docs/adr/0037-merge-pigeon-tf-terraform-modules.md)
for the merge.
