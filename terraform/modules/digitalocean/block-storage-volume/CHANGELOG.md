# Changelog

All notable changes to this module are documented in this file.

The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

## [0.1.0] - 2026-09-26

### docs(adr-0037): merge pigeon-tf terraform modules into this repo

## Context

`pigeon-tf` (a separate repo of versioned, reusable Terraform modules) is merged into this repo, continuing the monorepo direction ADR-0036 started.

## Decision

- `pigeon-tf`'s modules become `terraform/modules/{digitalocean,scaleway}/*`, with full commit history and all 20 release tags preserved via a `git filter-repo` path rewrite + `git merge --allow-unrelated-histories` (no squash, no snapshot copy).
- Its 12 ADRs join this repo's `docs/adr/` renumbered to 0038-0049, each with an added Origin bullet and internal cross-references/path citations rewritten to match, occurrence-by-occurrence (per ADR-0036's own precedent).
- Its two GitHub Actions workflows (`module-docs.yml`, `module-release.yml`) and one Claude Code skill (`release-pr`) move over unchanged in path but are retargeted at `terraform/modules/`.
- Its README becomes `terraform/README.md`; its CLAUDE.md folds into this repo's root CLAUDE.md.
- Each module's `CHANGELOG.md` is consolidated to a single 1.0.0 entry summarizing current capabilities, ahead of cutting fresh `terraform/modules/<provider>/<module>/v1.0.0` releases for all 9 modules.

Full design: [docs/adr/0037-merge-pigeon-tf-terraform-modules.md](https://github.com/noisypigeon/pigeon-cli/blob/adr-0037-merge-pigeon-tf/docs/adr/0037-merge-pigeon-tf-terraform-modules.md)

## Test plan

- [x] `mise run ci` passes (Rust-only gate, unaffected by this change)
- [x] `git log --follow` on a moved module file shows real pre-merge history
- [x] All 20 imported tags resolve against the rewritten paths

[#57](https://github.com/noisypigeon/pigeon-cli/pull/57)

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
