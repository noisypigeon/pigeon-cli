# Changelog

All notable changes to this module are documented in this file.

The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

## [0.1.1] - 2026-09-23

### Pin droplet's access-key dependency to a tagged version

Replaces droplet's local `../access-key` relative-path module source with an explicit remote git source pinned to `digitalocean/access-key/v0.1.0`, so droplet's access-key dependency is versioned independently instead of implicitly floating with whatever commit droplet itself is checked out at. This partially supersedes ADR-0004's original relative-path decision (not rewritten here) — worth a follow-up note there.

Also drops `region`'s default (now a required input — confirmed as an intentional breaking change, tagged as a patch release anyway since nothing consumes this module yet) and fixes two `droplet.tf` references to the module block, which was renamed from `compute_bucket_access` to `compute_bucket_access_key` (would otherwise have failed `terraform validate` with an undeclared-module reference).

[#10](https://github.com/noisypigeon/pigeon-tf/pull/10)

## [0.1.0] - 2026-09-23

### Port droplet module from pigeon-pizza

Ports `digitalocean/droplet` from `pigeon-pizza/tofu/modules/digitalocean/droplet` (per ADR-0004): a DigitalOcean droplet with cloud-init provisioning (rclone, an LVM auto-combine script for attached volumes, a sudo user) and a Cloudflare DNS alias.

- Drops the injected `source_do_access_key_module` variable for a plain relative-path `source = "../access-key"` — more self-contained, consistent with ADR-0002.
- New `versions.tf`: `digitalocean ~> 2.0`, `cloudflare ~> 5` (matches pigeon-do's ADR-0004 pin), `random ~> 3.0`.
- Tightened every input/output description to match this repo's established style; added descriptions to the two previously-undocumented `lvm_mount_point`/`lvm_filesystem` variables.
- Renamed the `url` output to `hostname` — its value is a bare hostname (`abc123.pigeon.dev`), not a URL.

`pigeon-pizza`'s own copy of this module is untouched — this is a copy, not a move. Not yet wired into any `pigeon-do` leaf stack.

[#9](https://github.com/noisypigeon/pigeon-tf/pull/9)
