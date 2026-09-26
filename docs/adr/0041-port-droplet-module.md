# ADR-0041: port the droplet module from pigeon-pizza

- **Author**: Willow Finch ([@noisypigeon](https://github.com/noisypigeon)).
- **Date**: 2026-09-23.
- **Status**: Accepted.
- **Origin**: pigeon-tf ADR-0004, merged into this repo by ADR-0037.

## Context

`pigeon-pizza/tofu/modules/digitalocean/droplet` is a real, working DigitalOcean droplet module — cloud-init provisioning (rclone, an LVM auto-combine script for attached volumes, a sudo user), a Cloudflare DNS alias — but it lives in `pigeon-pizza`'s `topology-v1`-style central `env.tf`/`source_do_x_module` pattern, the exact thing `pigeon-tf`'s ADR-0039 replaced with self-contained, independently-versioned modules. It isn't yet instantiated by any real leaf stack in `pigeon-pizza` (only the unused `source_do_droplet_module` path local exists there), so this is a copy into a better home, not an extraction from live infrastructure. `pigeon-pizza`'s own copy is untouched by this change.

This is the first `pigeon-tf` module needing two providers (DigitalOcean *and* Cloudflare) plus `random`, and the first with a genuine inner-module dependency — on `access-key`, for the droplet's rclone Spaces credentials.

## Decision

### Module source: drop the injected path variable

The source module's `spaces_key.tf` took the `access-key` module's path as `var.source_do_access_key_module`, set by the consumer via `pigeon-pizza`'s central `env.tf` — the pattern ADR-0039 already moved away from. Since `droplet` and `access-key` now live in the same repo at a fixed relative offset, the port hardcodes `source = "../access-key"` instead — no variable, no consumer-side wiring. Terraform resolves a local path module source relative to the `.tf` file that declares it, never relative to however the outer module itself was sourced, so this resolves correctly regardless of how `pigeon-do` references `droplet`. More self-contained than the pattern being replaced, directly in ADR-0039's spirit.

### Providers

`versions.tf` declares `digitalocean/digitalocean ~> 2.0` (matches every existing module), `cloudflare/cloudflare ~> 5` (exact-string match to both `pigeon-pizza`'s own pin and `pigeon-do`'s ADR-0004 pin — not a guess), and `hashicorp/random ~> 3.0` (matches `standard-storage-bucket`'s existing pin for the same `random_string.suffix` pattern).

### No module rename

`droplet` already names the DO resource concept directly, unlike `object-bucket`/`object-bucket-cold` (renamed in this repo's ADR-0040 because those names encoded a Terraform implementation detail instead of the actual product choice). Nothing analogous applies here.

### Input/output cleanup

Every input/output description was tightened to this repo's established terse, noun-phrase style (no leading articles, no periods, `(true/false)` parenthetical for booleans) — e.g. `"The name of the droplet."` → `"Droplet name suffix"`. The two previously-undocumented `lvm_mount_point`/`lvm_filesystem` variables gained real descriptions. `source_do_access_key_module` was removed outright (see above) rather than just re-described.

One output was actually renamed, not just re-described: `url`'s value is `"${random_string.suffix.result}.${data.cloudflare_zone.zone.name}"` — e.g. `abc123.pigeon.dev`, a hostname with no scheme or path, not a URL. Calling it `url` invites misuse (e.g. double-prefixing `https://`). Renamed to `hostname`.

No other variable or output *name* changed — `namespace`/`size`/`cloudflare_zone_id`/`ssh_key_name`/`buckets` already match sibling-module convention, and `public_networking` was deliberately left alone since it's a direct pass-through of `digitalocean_droplet`'s own argument name; diverging from that would hurt more than a terser name would help.

## Consequences

- `pigeon-tf` now has a real, deployable compute module, self-contained like every other module here.
- `droplet`'s consumed `access-key` revision is pinned to whatever commit `droplet` itself is checked out at — there's no way to float `access-key`'s version independently under a fixed `droplet` version. This is the whole-repo-checkout consumption model ADR-0039 already established, just exercised for the first time by real cross-module composition within this repo.
- Any consumer of the old `url` output name (none exist yet — this module isn't wired into `pigeon-do`) would need to update to `hostname`.

## Out of scope

- The cloud-init script references attached LVM volumes (`/dev/disk/by-id/scsi-0DO_Volume_...`) but this module has no `volume_ids`/`volumes` input — volumes are apparently attached some other way outside it. This is a verbatim port of the source module's behavior; its volume-attachment story wasn't reviewed or redesigned here.
- Any change to `pigeon-pizza` itself — its copy of this module, and its `env.tf` `source_do_droplet_module` entry, are both left exactly as they are.
- Wiring this module into any `pigeon-do` leaf stack.
