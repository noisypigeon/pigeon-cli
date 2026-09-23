# Changelog

All notable changes to this module are documented in this file.

The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

## [0.1.0] - 2026-09-23

### Rename object-bucket to standard-storage-bucket

Renames `digitalocean/object-bucket` to `digitalocean/standard-storage-bucket` (per ADR-0003) — names the module after what it actually is (standard object storage, as opposed to Cold Storage) rather than an implementation detail. Also tightens the `project` input and `name` output descriptions to match sibling-module tone.

Breaking for any consumer referencing the old path directly — notably `pigeon-do`'s `pigeon.dev/digitalocean/tor1/management/terraform-state/bucket.tf`, which is not fixed in this PR (see ADR-0003's explicit deferral).

No functional/behavior change otherwise — same resources, same inputs/outputs schema.

[#5](https://github.com/noisypigeon/pigeon-tf/pull/5)
