# ADR-0054: pigeon-tf modules repo scaffold

- **Author**: Willow Finch ([@noisypigeon](https://github.com/noisypigeon)).
- **Date**: 2026-09-22.
- **Status**: Accepted.
- **Origin**: `pigeon-do` ADR-0002.

## Context

ADR-0053 bootstrapped `pigeon-do`'s Terragrunt/Terraform toolchain, `root.hcl`, and secrets handling, but explicitly deferred module design. `pigeon-do` still has no real leaf stacks because there's nowhere for them to pull reusable module code from.

`topology-v1` already solved this once, in-repo: a `modules/digitalocean/{access-key,block-volume,droplet,object-bucket,project,resource-project-attachment}` tree, referenced from leaf `.tf` files via ordinary Terraform `module "x" { source = local.source_do_x_module ... }` blocks, e.g.:

```hcl
module "management" {
  source      = local.source_do_project_module
  environment = local.production
  name        = local.management
  purpose     = "Operational / Developer tooling"
}
```

Each `source_do_x_module` local is a hand-maintained entry in the repo-root `env.tf`, generated into every leaf, e.g. `source_do_project_module = "${local.repo_root}/modules/digitalocean/project"`. Every new module means editing that central file.

Three of those modules are being ported as the starting point for a new, dedicated modules repo:

- **`object-bucket`** — `digitalocean_spaces_bucket` plus a `random_string` suffix for name uniqueness.
- **`project`** — thin wrapper around `digitalocean_project`.
- **`access-key`** — `digitalocean_spaces_key` with a `dynamic "grant"` block.

None of these three declare their own `required_providers` — they rely entirely on `topology-v1`'s root-level `providers.tf`/`root.hcl` injecting the `digitalocean` provider centrally. `object-bucket` additionally uses `random_string`, which isn't declared anywhere and relies on Terraform's implicit provider inference. A module that depends on the *consuming* repo happening to declare the right providers isn't actually reusable on its own.

This ADR decides how to scaffold a new, separate public repo (`noisypigeon/pigeon-tf`) to hold versioned, reusable modules going forward, and how `pigeon-do` will consume them locally — no go-getter/network module source, since Terragrunt only ever runs locally for now.

## Decision

### `pigeon-tf` repo layout

- New public repo, `noisypigeon/pigeon-tf`. Root holds provider/resource directories directly — `digitalocean/access-key`, `digitalocean/object-bucket`, `digitalocean/project` — with **no wrapping `modules/` directory**, since the whole repo already is a module collection. This simplifies on `topology-v1`'s nested layout without losing any structure (still one directory per provider, one per resource).
- Starter files:
  - `README.md` — repo purpose, an index of available modules, and the versioning convention (below).
  - `.gitignore` — `.terraform/`, `*.tfstate*`, `.terraform.lock.hcl`, `.DS_Store`.
- No `LICENSE` file — matches the existing convention across `pigeon-cli` and `topology-v1`, both public with none.
- No root provider/backend configuration — this repo is never `terragrunt`/`terraform` run standalone, only consumed as module sources by other repos.

### Ported modules — self-containment fix

- Each of the three modules is copied over with its resources, variables, and outputs unchanged, but gains its own `versions.tf` declaring `required_providers` for what it actually uses:
  - `digitalocean/digitalocean`, `~> 2.0` (all three — matches `topology-v1`'s existing constraint) — plus
  - `hashicorp/random`, `~> 3.0` (`object-bucket` only, for `random_string.suffix`).
- This closes the gap identified in Context: a module consumed from a different root config no longer silently depends on that repo's `root.hcl` happening to inject the right provider.

### Versioning

- Semantic-version git tags on `pigeon-tf` — `vX.Y.Z`.
- Initial release: `v0.1.0`, covering the three ported modules.
- No CI or release automation — deferred, consistent with ADR-0053 also deferring CI.

### Local sync (no network fetch)

- Contributors `git clone git@github.com:noisypigeon/pigeon-tf.git` as a sibling of `pigeon-do` (i.e. `../pigeon-tf`) and `git checkout` whichever tag they want to work against.
- Nothing in `pigeon-do` automates or enforces this — no submodule, no sync script, no lockfile.
- This is a deliberate simplicity-over-enforcement tradeoff: version drift between what's tagged and what's actually checked out locally is possible and won't be caught automatically. Acceptable because Terragrunt is only ever run locally today; revisit (submodule, a sync task, an explicit pinned-version file) if that stops being true.

### Import path normalization in `pigeon-do`

- `root.hcl` gains one new local, `pigeon_tf_root`, resolved as:
  ```hcl
  pigeon_tf_root = get_env("PIGEON_TF_PATH", "${dirname(get_repo_root())}/pigeon-tf")
  ```
  (env var override, sibling-directory default), generated into every leaf as a real Terraform local via a new `generate` block — the same mechanism `root.hcl` already uses for `generate "provider"`/`generate "env"`.
- Leaf `.tf` files then reference a module by interpolating this one local directly at the call site, instead of a central per-module named local:
  ```hcl
  module "management" {
    source = "${local.pigeon_tf_root}/digitalocean/project"
    ...
  }
  ```
- This is the improvement over `topology-v1`'s pattern: adding a new module in `pigeon-tf` needs **zero** changes on the `pigeon-do` side — no central `env.tf` registry to keep in sync. The tradeoff is that there's no longer one file listing every module currently in use across the repo; that's judged an acceptable cost for a module set this size.

## Consequences

- `pigeon-do` can start writing real leaf stacks once `pigeon-tf` exists and is cloned locally.
- New modules in `pigeon-tf` require no `pigeon-do`-side changes to reference — just the convention-based path.
- Version drift (tagged vs. locally checked-out) is possible and not automatically caught — accepted tradeoff of the manual-clone sync model.

## Out of scope

- CI, module registry publishing, or automated version-pin enforcement for `pigeon-tf`.
- Additional modules beyond the three ported here — notably not `droplet`, `block-volume`, or `resource-project-attachment` from `topology-v1`; port those in a future ADR/PR if/when needed.
- Any actual `pigeon-do` leaf stacks — this ADR only makes modules available to reference, it doesn't reference them yet.
