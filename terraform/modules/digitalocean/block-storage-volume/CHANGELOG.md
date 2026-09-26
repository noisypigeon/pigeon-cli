# Changelog

All notable changes to this module are documented in this file.

The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

## [1.0.0] - 2026-09-25

### Consolidate as terraform/modules/digitalocean/block-storage-volume 1.0.0

Provisions one or more DigitalOcean Block Storage volumes
(`digitalocean_volume`) and attaches them to a droplet
(`digitalocean_volume_attachment`). Each volume can be pre-formatted with a
filesystem, or left unformatted for downstream combination via LVM/mdadm on
the droplet — the `device_ids` output exposes stable by-id device paths
(`/dev/disk/by-id/scsi-0DO_Volume_<name>`) for that purpose. See
[ADR-0042](../../../../docs/adr/0042-add-block-storage-volume-module.md)
for the full set of decisions behind this module's design.

Consolidates this module's prior `pigeon-tf` version history (`v0.1.0`) into
a single 1.0.0 release as part of merging `pigeon-tf` into this repo — see
[ADR-0037](../../../../docs/adr/0037-merge-pigeon-tf-terraform-modules.md)
for the merge.
