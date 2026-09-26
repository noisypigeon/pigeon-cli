# Changelog

All notable changes to this module are documented in this file.

The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

## [0.1.0] - 2026-09-26

### Consolidate as terraform/modules/digitalocean/cold-storage-bucket 0.1.0

A data-source wrapper for a DigitalOcean Spaces Cold Storage bucket (not yet
supported as a Terraform resource by the DigitalOcean provider — the bucket
is created click-ops and managed here as a data source), optionally attached
to a project. Renamed from `object-bucket-cold` (per
[ADR-0040](../../../../docs/adr/0040-storage-bucket-module-renames.md)) to
name the module after the actual consumer choice rather than a Terraform
implementation detail.

Consolidates this module's prior `pigeon-tf` version history (`v0.1.0`) into
a single 0.1.0 release as part of merging `pigeon-tf` into this repo — see
[ADR-0037](../../../../docs/adr/0037-merge-pigeon-tf-terraform-modules.md)
for the merge.
