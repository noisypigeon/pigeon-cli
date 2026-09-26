# ADR-0053: Terragrunt/Terraform bootstrap

- **Author**: Willow Finch ([@noisypigeon](https://github.com/noisypigeon)).
- **Date**: 2026-09-22.
- **Status**: Accepted.
- **Origin**: `pigeon-do` ADR-0001.

## Context

`pigeon-do` manages `pigeon.dev`'s infrastructure (DigitalOcean + Cloudflare) as code, via Terragrunt wrapping Terraform. It succeeds [`topology-v1`](https://github.com/noisypigeon/topology-v1) (private repo), which currently does the same for both `noisypigeon.com` and `pigeon.dev` from one repo. That repo's `root.hcl` established a working pattern: `generate` blocks for provider configuration and a DigitalOcean Spaces remote-state backend, plus an `env.tf` ancestor-inheritance mechanism so any directory level can hand shared locals down to its descendants.

The repo currently has an empty `root.hcl`, `.gitignore`, `CLAUDE.md`, and `README.md`, plus directory scaffolding (`pigeon.dev/cloudflare/global`, `pigeon.dev/digitalocean/{fra1,tor1,global}`) with no `.tf`/`.hcl` content and no commits yet. Before any real resource lands, the toolchain, `root.hcl` design, and secrets handling need to be decided — the last one especially carefully, since this repo is public and nothing has been pushed yet, so this is the only chance to get `.gitignore` right before any secret could be exposed.

Unlike `topology-v1`, `pigeon-do` only scaffolds a single domain/DO team (`pigeon.dev`), not two — so the secrets model below is simpler than topology-v1's root+tree override chain.

## Decision

### Toolchain (`mise`)

- [Mise](https://mise.jdx.dev) manages the `terraform` and `terragrunt` binaries via `.mise.toml`, the same role it plays for the Rust toolchain in `pigeon-cli` (ADR-0002/0004 there). `mise install` gives any contributor the exact same pinned versions — no system-wide `tfenv`/`tgenv` or manually-managed binaries.
- Versions are pinned to whatever's latest stable at implementation time (`mise use terraform@latest terragrunt@latest`), then locked as exact versions in the committed `.mise.toml` — not invented ahead of time in this ADR.
- Mise tasks front the common workflows (e.g. `fmt`, `plan`, `apply`, each wrapping the equivalent `terragrunt`/`terraform` invocation) as the single documented entry point per workflow, mirroring `pigeon-cli` ADR-0004's rationale: `mise run plan` reads as what it does, and keeps `terragrunt run-all plan`-style incantations from needing to be memorized or re-discovered.

### `root.hcl` setup

- One `root.hcl` at the repo root — no per-domain root configs, since there's only one domain here.
- Reuses `topology-v1`'s `generate` pattern:
  - a `provider` block for `digitalocean` and `cloudflare`, credentials sourced per the secrets model below;
  - a `remote_state` block backed by a DigitalOcean Spaces bucket (S3-compatible, `tor1` region), state key derived from `path_relative_to_include()` exactly as before.
- Reuses the `env.tf` ancestor-inheritance mechanism: any directory between a leaf and the repo root may define its own `env.tf` (e.g. a future `pigeon.dev/env.tf` for domain-wide constants, `pigeon.dev/digitalocean/fra1/env.tf` for that region's), and every matching ancestor level is generated into the leaf, not just the nearest one. This is already anticipated by the existing `pigeon.dev/digitalocean/{fra1,tor1,global}` layout.
- Deviates from `topology-v1` in one respect: secrets resolve from a single root `.env` only (no tree-level override chain) — see below.

### Secrets management (public repo)

- A single root-level `.env` (git-ignored, never committed) holds:
  ```
  CLOUDFLARE_TOKEN=
  DIGITALOCEAN_TOKEN=
  DIGITALOCEAN_SPACES_ACCESS_ID=
  DIGITALOCEAN_SPACES_SECRET_KEY=
  DIGITALOCEAN_TERRAFORM_STATE_BUCKET=
  ```
- A committed `.env.example` documents these same keys with blank values, so a new contributor knows exactly what to fill in without ever seeing a real value.
- `root.hcl` reads each secret via `get_env("KEY", lookup(local.secrets, "KEY", ""))`, where `local.secrets` is parsed from the root `.env`. A real shell environment variable always wins over the `.env` file — useful for CI or one-off overrides without editing the file.
- `.gitignore` excludes `.env`, `.terragrunt-cache/`, `.terraform.lock.hcl`, and `.DS_Store` from the very first commit. Since the repo has no commits yet, this ordering matters: `.gitignore` must be correct *before* anything is ever `git add`ed, not patched in afterward.
- No tree-level `.env` override chain (unlike `topology-v1`) — with a single domain/team, one root `.env` is sufficient. If a second domain or DO team is added later, revisit this decision rather than retrofitting it silently.

### Modules — deferred

- No `modules/` directory or resource modules are designed or created by this ADR.
- The expected future convention is `modules/<provider>/<resource>` (mirroring `topology-v1`'s `modules/digitalocean/{access-key,block-volume,droplet,object-bucket,project,resource-project-attachment}`), but this ADR does not commit to that shape.
- Actual module design is deferred to a follow-up ADR, written once the first real resource (e.g. the first droplet or DNS record) is implemented — consistent with how `pigeon-cli`'s ADR-0002 backfilled scaffolding decisions rather than speculating on them upfront.

## Consequences

- Contributors get one documented, reproducible way to install the toolchain (`mise install`) and run Terragrunt workflows (`mise run <task>`).
- `root.hcl` is functional for a single domain/team from day one, with a clear, minimal secrets story appropriate for a public repo.
- Adding `pigeon.dev`'s actual DigitalOcean/Cloudflare resources, or a second domain/team, both require follow-up decisions (modules, and possibly the root+tree secrets chain) not made here.

## Out of scope

- Actual DigitalOcean/Cloudflare resource definitions for `pigeon.dev`.
- CI wiring (e.g. GitHub Actions running `terragrunt plan`/`apply`).
- Migrating or importing any existing state/resources from `topology-v1`.
