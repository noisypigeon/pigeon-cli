# Changelog

All notable changes to this module are documented in this file.

The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

## [0.1.0] - 2026-09-26

### Consolidate as terraform/modules/digitalocean/standard-storage-bucket 0.1.0

A DigitalOcean Spaces bucket (`digitalocean_spaces_bucket`) named with a
randomized suffix scheme (`{namespace}-{random_code}-{name}`, e.g.
`example-com-q82q17-sample`). Renamed from `object-bucket` (per
[ADR-0040](../../../../docs/adr/0040-storage-bucket-module-renames.md)) to
name the module after the actual consumer choice (standard object storage
vs. Cold Storage) rather than a Terraform implementation detail.

Consolidates this module's prior `pigeon-tf` version history (`v0.1.0`) into
a single 0.1.0 release as part of merging `pigeon-tf` into this repo — see
[ADR-0037](../../../../docs/adr/0037-merge-pigeon-tf-terraform-modules.md)
for the merge.
