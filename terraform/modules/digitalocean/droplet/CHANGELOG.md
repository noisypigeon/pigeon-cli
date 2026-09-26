# Changelog

All notable changes to this module are documented in this file.

The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

## [0.1.0] - 2026-09-26

### Consolidate as terraform/modules/digitalocean/droplet 0.1.0

A DigitalOcean droplet (`digitalocean_droplet`) with cloud-init provisioning
(rclone, an LVM auto-combine script for attached volumes, a sudo user) and a
Cloudflare DNS alias (`hostname` output — a bare hostname, not a URL). Ported
from `pigeon-pizza` (per
[ADR-0041](../../../../docs/adr/0041-port-droplet-module.md)), with its
`access-key` dependency pinned to an explicit tagged git ref rather than a
floating relative-path source.

Consolidates this module's prior `pigeon-tf` version history (`v0.1.0`,
`v0.1.1`) into a single 0.1.0 release as part of merging `pigeon-tf` into
this repo — see
[ADR-0037](../../../../docs/adr/0037-merge-pigeon-tf-terraform-modules.md)
for the merge.
