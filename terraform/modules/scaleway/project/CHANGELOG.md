# Changelog

All notable changes to this module are documented in this file.

The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

## [1.0.0] - 2026-09-25

### Consolidate as terraform/modules/scaleway/project 1.0.0

A thin wrapper around `scaleway_account_project`, mirroring
`terraform/modules/digitalocean/project`'s layout and passthrough style —
this was `pigeon-tf`'s first module under the `scaleway/` provider root
(documented in
[ADR-0043](../../../../docs/adr/0043-add-scaleway-provider.md)), which also
generalized the release automation beyond a single hardcoded provider root.

Consolidates this module's prior `pigeon-tf` version history (`v0.1.0`) into
a single 1.0.0 release as part of merging `pigeon-tf` into this repo — see
[ADR-0037](../../../../docs/adr/0037-merge-pigeon-tf-terraform-modules.md)
for the merge.
