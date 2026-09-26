# ADR-0040: standard/cold storage bucket module renames

- **Author**: Willow Finch ([@noisypigeon](https://github.com/noisypigeon)).
- **Date**: 2026-09-23.
- **Status**: Accepted.
- **Origin**: pigeon-tf ADR-0003, merged into this repo by ADR-0037.

## Context

`terraform/modules/digitalocean/object-bucket` and `terraform/modules/digitalocean/object-bucket-cold` name the two bucket modules after an implementation detail (whether Terraform manages the bucket as a resource or a data source) rather than what a consumer actually chooses between: standard object storage vs. Cold Storage. `standard-storage-bucket` / `cold-storage-bucket` name that choice directly and read as a matched pair.

This ADR also tightens input/output descriptions on both renamed modules and on `project` to match the terse, noun-phrase style already established elsewhere in this repo (e.g. `access-key`'s recent cleanup, `object-bucket`'s existing `"Bucket region"`).

Research before this change found that `pigeon-do` is not still at the scaffolding stage its own earlier ADRs (0001/0002) implied — it now has real, tracked leaf `.tf` files referencing these modules by path:
- `pigeon.dev/digitalocean/tor1/management/terraform-state/bucket.tf` → `source = "${local.pigeon_tf_root}/digitalocean/object-bucket"`
- `pigeon.dev/digitalocean/tor1/rolodex/email/bucket/bucket.tf` → `source = "${local.pigeon_tf_root}/digitalocean/object-bucket-cold"`

`pigeon_tf_root` resolves to a sibling-directory clone with no pin in `pigeon-do`'s current `.env` (no `PIGEON_TF_PATH` override) — in this environment it resolves directly to this same working copy, so there's no tag-pinning cushion. **Renaming these two directories breaks those two `pigeon-do` leaves on their next `terragrunt plan`/`apply`, not eventually.**

## Decision

- Rename `terraform/modules/digitalocean/object-bucket` → `terraform/modules/digitalocean/standard-storage-bucket`, `terraform/modules/digitalocean/object-bucket-cold` → `terraform/modules/digitalocean/cold-storage-bucket`, via `git mv` (preserves file history).
- Tighten each renamed module's `project` input description and `name` output description to match sibling-module tone; tighten all of `project`'s input/output descriptions likewise. No `type`/`default`/behavior changes anywhere.
- Update `.github/workflows/module-docs.yml`'s hardcoded `working-dir:` list (both renamed paths) and `terraform/README.md`'s module table — both would otherwise point at directories that no longer exist.
- Each rename ships as its own PR labeled `release:major` — a path/identity rename is a breaking change for any existing consumer referencing the module by source path, confirmed true for `pigeon-do` above, even though the module's own input/output schema is unchanged.
- **The `pigeon-do`-side fix (updating the two `source =` lines) is explicitly deferred to a separate follow-up task, not done here.** This is a deliberate choice, not an oversight — recorded here so the breakage is discoverable via this repo's own "ADRs govern this project" convention rather than only living in chat history.

## Consequences

- `standard-storage-bucket` and `cold-storage-bucket` read as a matched pair naming the actual choice a consumer makes, instead of an implementation detail.
- `pigeon-do`'s `terraform-state` and `rolodex/email/bucket` leaves reference nonexistent module paths until their `source =` lines are updated in a follow-up `pigeon-do` change. Any `terragrunt plan`/`apply` against those leaves fails until that happens.
- `module-docs.yml`'s `working-dir:` list needed a manual edit per rename — a reminder that this list doesn't derive itself from the directory tree the way `module-release.yml`'s module discovery already does; a future ADR could revisit that if renames become more frequent.

## Out of scope

- Updating `pigeon-do`'s `source =` paths (deferred, see above).
- Renaming `access-key` or `project` — neither name is tied to an implementation detail the way `object-bucket`/`object-bucket-cold` were.
- Making `module-docs.yml`'s module list self-discovering instead of hardcoded.
