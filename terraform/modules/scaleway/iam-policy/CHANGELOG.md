# Changelog

All notable changes to this module are documented in this file.

The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

## [0.1.0] - 2026-09-26

### Consolidate as terraform/modules/scaleway/iam-policy 0.1.0

A Scaleway `scaleway_iam_application` and `scaleway_iam_policy` wrapper that
produces a permission-scoped `scaleway_iam_api_key`. Access can be granted
independently at three scopes — organization (`organization_id` +
`organization_permission_sets`), project (`project_ids` +
`project_permission_sets`), and specific Object Storage buckets
(`bucket_names`, a map of static logical key → bucket name, +
`bucket_actions`, validated against a known set of S3 actions) — with at
least one scope required but none individually mandatory; the underlying
`scaleway_iam_policy` resource and its `rule` blocks are created only when a
scope is fully populated. An optional `expires_at` input sets an expiration
timestamp on the minted API key. Requires Terraform/OpenTofu `>= 1.9.0` for
its cross-variable `validation` block. See
[ADR-0046](../../../../docs/adr/0046-add-scaleway-iam-policy-module.md)
through
[ADR-0049](../../../../docs/adr/0049-scaleway-iam-policy-optional-scopes.md)
for the full set of decisions behind this module's design.

Consolidates this module's prior `pigeon-tf` version history (`v0.1.0`
through `v3.0.1`) into a single 0.1.0 release as part of merging `pigeon-tf`
into this repo — see
[ADR-0037](../../../../docs/adr/0037-merge-pigeon-tf-terraform-modules.md)
for the merge.
