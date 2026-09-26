# ADR-0043: add the scaleway provider root

- **Author**: Willow Finch ([@noisypigeon](https://github.com/noisypigeon)).
- **Date**: 2026-09-25.
- **Status**: Accepted.
- **Origin**: pigeon-tf ADR-0006, merged into this repo by ADR-0037.

## Context

`pigeon-tf` has been DigitalOcean-only since ADR-0038, and its release automation reflects that: `module-docs.yml`'s trigger path and `module-release.yml`'s module discovery are both hardcoded to the literal `terraform/modules/digitalocean` root. Worse, `module-release.yml`'s tag lookup and tag creation reconstruct each module's tag as `digitalocean/<module>/v<version>` from a bare basename, rather than using the already-correct relative module path it discovers — a latent bug that would silently mis-tag any non-DigitalOcean module (e.g. tagging a new `scaleway/project` module as `digitalocean/project/v0.1.0`, colliding in spirit with the real `digitalocean/project` module's own tag sequence).

Adding Scaleway as a second provider, starting with a module wrapping `scaleway_account_project` (the Scaleway equivalent of the existing `terraform/modules/digitalocean/project` thin wrapper), forces both the generalization and the bug fix to happen together — the module can't be safely released otherwise.

## Decision

### New module: `scaleway/project`

A thin wrapper around `scaleway_account_project`, mirroring `terraform/modules/digitalocean/project`'s exact file layout and passthrough style (`main.tf` / `inputs.tf` / `outputs.tf` / `versions.tf` / `README.md`, no hand-written `CHANGELOG.md`). Named `project`, not `account-project`, per ADR-0040's "name modules after the consumer-facing choice, not the implementation detail" principle — it's the direct Scaleway analog of the existing `terraform/modules/digitalocean/project` module. All three of the underlying resource's arguments (`name`, `description`, `organization_id`) are optional, so all three module inputs default to `null`. Outputs are kept minimal (`id`, `name`), matching `terraform/modules/digitalocean/project`'s existing minimal-output philosophy rather than exposing every computed attribute (`created_at`/`updated_at` are left out; can be added later if a consumer needs them).

### Provider pin

`scaleway/scaleway ~> 2.0` in `versions.tf` — same pessimistic-constraint convention as every `digitalocean` module (current latest is 2.83.1; `~> 2.0` tracks any 2.x release).

### Generalizing `module-docs.yml`

The trigger `paths:` filter changes from the DigitalOcean-only `terraform/modules/digitalocean/**/*.tf` to a repo-wide `**/*.tf` — safe because ADR-0038 guarantees no `.tf` files exist outside module directories, so this can't over-trigger. The `working-dir:` input stays a hand-maintained comma-separated list (ADR-0040 already accepted this as a manual-edit point for every new module, regardless of provider); `scaleway/project` is appended to it.

### Generalizing `module-release.yml`

Module discovery (`find terraform/modules/digitalocean -mindepth 2 -maxdepth 2 -name versions.tf`) becomes `find terraform/modules/digitalocean terraform/modules/scaleway -mindepth 2 -maxdepth 2 -name versions.tf` — an explicit, hand-maintained provider-root list, consistent with this repo's existing preference for explicit lists over dynamic scanning (matches the `working-dir:` precedent above). This list gets one more entry on each future new provider.

### Bug fix: tag naming

The tag-lookup and tag-creation lines reconstructed `digitalocean/${MODULE}/v*` from a bare basename (`$MODULE`), rather than using `${dir}` — the full relative module path already computed earlier in the same job step, which is correct for any provider root. Both lines now use `${dir}` directly. For every existing DigitalOcean module, `${dir}` already equals `terraform/modules/digitalocean/<module>`, so this changes no observable behavior for them; it just removes a bug that would otherwise misfire on the first non-DigitalOcean module.

### Repo-scope documentation

`pigeon-tf`'s own `CLAUDE.md` (later folded into this repo's root `CLAUDE.md` by ADR-0037) had its scope line changed from "(DigitalOcean)" to "(DigitalOcean, Scaleway)", with a new bullet added under "ADRs govern this project" pointing at this ADR. `terraform/README.md`'s Modules table gains a `scaleway/project` row.

## Consequences

- `pigeon-tf` becomes genuinely multi-provider, with a documented, repeatable pattern for adding the next one.
- The tag-naming fix removes a latent bug before it could ever fire, at the cost of zero behavior change for existing modules.

## Out of scope

- Wiring `scaleway/project` into any `pigeon-do` stack.
- Any other Scaleway resource or module beyond this first one.
- Consolidating `pigeon-do`'s single whole-repo `pigeon-tf` pin into a per-module scheme — ADR-0039 already flagged this as a separate, deferred problem, and it applies equally regardless of how many providers `pigeon-tf` has.
