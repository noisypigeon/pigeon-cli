# Changelog

All notable changes to this module are documented in this file.

The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

## [0.1.0] - 2026-09-26

### Consolidate as terraform/modules/digitalocean/access-key 0.1.0

A DigitalOcean Spaces access key (`digitalocean_spaces_key`), optionally
scoped to one or more buckets via `is_bucket_scoped` (`false` grants full
account access through an empty-string bucket grant; `true` restricts the
key to the named buckets).

Consolidates this module's prior `pigeon-tf` version history (`v0.1.0`) into
a single 0.1.0 release as part of merging `pigeon-tf` into this repo — see
[ADR-0037](../../../../docs/adr/0037-merge-pigeon-tf-terraform-modules.md)
for the merge and
[ADR-0038](../../../../docs/adr/0038-pigeon-tf-scaffold.md) for this
module's original scaffold decisions.
