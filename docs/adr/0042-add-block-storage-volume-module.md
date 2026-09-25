# ADR-0005: add block-storage-volume module

- **Author**: Willow Finch ([@noisypigeon](https://github.com/noisypigeon)).
- **Date**: 2026-09-25.
- **Status**: Accepted.

## Context

`block-storage-volume` already existed in the working tree as an untracked, ad hoc module (`inputs.tf`, `outputs.tf`, `volume.tf`) with no git history — missing the scaffolding every sibling module has (`versions.tf`, `README.md`), input/output descriptions predating ADR-0004's terse noun-phrase convention, and a copy-paste bug in its `urns` output. This ADR documents bringing it up to repo scaffold conventions before its first release, and the decisions made while doing so.

## Decision

### Scope

The module provisions one or more `digitalocean_volume`s and attaches them to a droplet via `digitalocean_volume_attachment`. Each volume can optionally be pre-formatted with a filesystem, or left unformatted (`initial_filesystem_type = null`) for downstream combination via LVM/mdadm on the droplet — the `device_ids` output exposes stable by-id device paths (`/dev/disk/by-id/scsi-0DO_Volume_<name>`) for that purpose.

### Providers

Only `digitalocean/digitalocean ~> 2.0` — unlike `droplet`, this module needs no `random` or `cloudflare` provider.

### Bug fix: `urns` output

The `urns` output's `value` was `digitalocean_volume.volume[*].id`, copy-pasted from the `ids` output, instead of `digitalocean_volume.volume[*].urn`. Fixed as part of this cleanup rather than filed separately, since the module has never been released and has no consumers to migrate.

### Input/output cleanup

Every input/output description was tightened to this repo's established terse, noun-phrase style (no leading articles, no trailing periods), matching the pattern ADR-0004 applied to `droplet`.

### Docs

Added `README.md` with `terraform-docs` inject markers (`<!-- BEGIN_TF_DOCS -->` / `<!-- END_TF_DOCS -->`), populated by CI after merge. Added the module to `module-docs.yml`'s hand-maintained `working-dir` list — ADR-0003 already flagged this list as non-self-discovering, so a new module is invisible to doc generation until added there. Added a row to the root `README.md` module table, per ADR-0001's rule that the table is the living, authoritative list of what's available.

### Changelog

Deliberately **not** hand-written. Every sibling module's `CHANGELOG.md` is created and maintained entirely by `module-release.yml`, starting from the module's first automated release — this module follows the same pattern rather than pre-seeding a file the automation will manage from here on.

### Release

Shipped via the `release-pr` skill, tagged `digitalocean/block-storage-volume/v0.1.0` with a `release:minor` label (new, backwards-compatible module addition).

## Consequences

- `pigeon-tf` gains a released, consumable `block-storage-volume` module.
- No prior consumers of the module existed, so the `urns` output fix has no migration impact.

## Out of scope

- Wiring this module into `droplet` or any `pigeon-do` leaf stack.
- The LVM/mdadm combination logic itself, which lives in `droplet`'s cloud-init (per ADR-0004's own out-of-scope note) — this module only provides the raw volumes and their stable device paths.
