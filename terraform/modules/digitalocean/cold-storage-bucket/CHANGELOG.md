# Changelog

All notable changes to this module are documented in this file.

The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

## [0.1.0] - 2026-09-23

### Rename object-bucket-cold to cold-storage-bucket

Renames `digitalocean/object-bucket-cold` to `digitalocean/cold-storage-bucket` (per ADR-0003) — names the module after what it actually is (Cold Storage) rather than an implementation detail. Also tightens the `project` input and `name` output descriptions to match sibling-module tone.

Breaking for any consumer referencing the old path directly — notably `pigeon-do`'s `pigeon.dev/digitalocean/tor1/rolodex/email/bucket/bucket.tf`, which is not fixed in this PR (see ADR-0003's explicit deferral).

No functional/behavior change otherwise — same resources, same inputs/outputs schema.

[#7](https://github.com/noisypigeon/pigeon-tf/pull/7)
