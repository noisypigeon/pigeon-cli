# Changelog

All notable changes to this module are documented in this file.

The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

## [0.1.0] - 2026-09-25

### Add block-storage-volume module

Adds a `block-storage-volume` module for provisioning one or more DigitalOcean Block Storage volumes (`digitalocean_volume`) and attaching them to a droplet (`digitalocean_volume_attachment`). Each volume can be pre-formatted with a filesystem, or left unformatted for downstream combination via LVM/mdadm on the droplet — the module's `device_ids` output exposes stable by-id device paths (`/dev/disk/by-id/scsi-0DO_Volume_<name>`) for that purpose.

This also fixes a bug in the module's `urns` output, which previously returned volume IDs instead of URNs, and tightens all input/output descriptions to this repo's terse noun-phrase convention. See ADR-0005 for the full set of decisions behind this module's design.

[#11](https://github.com/noisypigeon/pigeon-tf/pull/11)
