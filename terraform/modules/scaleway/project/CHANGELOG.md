# Changelog

All notable changes to this module are documented in this file.

The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

## [0.1.0] - 2026-09-25

### Add scaleway/project module

Adds `scaleway/project`, a thin wrapper around `scaleway_account_project` mirroring `digitalocean/project`'s layout and passthrough style — this is `pigeon-tf`'s first module under a new `scaleway/` provider root, making the repo genuinely multi-provider for the first time (documented in ADR-0006).

This also generalizes `module-docs.yml` and `module-release.yml`, which previously hardcoded `digitalocean` as the only provider root, and fixes a latent bug in `module-release.yml`'s tag naming: it reconstructed each module's release tag as `digitalocean/<module>/v<version>` from a bare basename rather than using the already-discovered module path, which would have silently mis-tagged any non-DigitalOcean module. Existing DigitalOcean modules are unaffected by this fix.

[#12](https://github.com/noisypigeon/pigeon-tf/pull/12)
