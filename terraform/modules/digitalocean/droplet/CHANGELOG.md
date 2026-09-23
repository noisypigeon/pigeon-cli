# Changelog

All notable changes to this module are documented in this file.

The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

## [0.1.0] - 2026-09-23

### Port droplet module from pigeon-pizza

Ports `digitalocean/droplet` from `pigeon-pizza/tofu/modules/digitalocean/droplet` (per ADR-0004): a DigitalOcean droplet with cloud-init provisioning (rclone, an LVM auto-combine script for attached volumes, a sudo user) and a Cloudflare DNS alias.

- Drops the injected `source_do_access_key_module` variable for a plain relative-path `source = "../access-key"` — more self-contained, consistent with ADR-0002.
- New `versions.tf`: `digitalocean ~> 2.0`, `cloudflare ~> 5` (matches pigeon-do's ADR-0004 pin), `random ~> 3.0`.
- Tightened every input/output description to match this repo's established style; added descriptions to the two previously-undocumented `lvm_mount_point`/`lvm_filesystem` variables.
- Renamed the `url` output to `hostname` — its value is a bare hostname (`abc123.pigeon.dev`), not a URL.

`pigeon-pizza`'s own copy of this module is untouched — this is a copy, not a move. Not yet wired into any `pigeon-do` leaf stack.

[#9](https://github.com/noisypigeon/pigeon-tf/pull/9)
