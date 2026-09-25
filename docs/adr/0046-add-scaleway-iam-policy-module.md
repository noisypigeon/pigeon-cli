# ADR-0009: add the scaleway/iam-policy module

- **Author**: Willow Finch ([@noisypigeon](https://github.com/noisypigeon)).
- **Date**: 2026-09-25.
- **Status**: Accepted.

## Context

`scaleway/iam-policy` is a new module wrapping `scaleway_iam_application`, `scaleway_iam_policy`, and `scaleway_iam_api_key` to produce a permission-scoped Scaleway API key: it creates an application, attaches a policy rule granting a set of `permission_set_names` (optionally restricted to `project_ids`), and mints an API key against that application. It follows the precedent of every prior Scaleway module addition (ADR-0006 for `project`, ADR-0007 for `object-bucket`) getting its own ADR.

The module was scaffolded as `scaleway/iam_policy/` (underscore), but every other module directory in this repo — `standard-storage-bucket`, `cold-storage-bucket`, `block-storage-volume`, `access-key`, `project`, `droplet`, `object-bucket` — uses kebab-case. This is the same class of naming inconsistency ADR-0003 addressed for its module, except here the module was never committed or released, so there is no consumer or tag history to break — a pure pre-release naming fix, not a breaking rename.

Two implementation bugs surfaced during review, both of which would block first use:
- `outputs.tf` referenced `scaleway_iam_api_key.terraform_key`, a resource label that doesn't exist — the actual resource in `iam_policy.tf` is labeled `api_key`. `terraform validate` fails as written.
- `versions.tf` was empty, missing the `required_providers` block every other module in this repo declares, per ADR-0001's self-containment convention.

Separately, all four inputs (`name`, `description`, `project_ids`, `permission_set_names`) defaulted to `null`. A policy `rule` with no `permission_set_names` grants nothing — an invocation that "succeeds" but produces a useless API key — and a `null` `name` produces literal resource names like `"null-application"`/`"null-policy"`, which is never a real intent.

## Decision

### Directory rename: `iam_policy` → `iam-policy`

Renamed for consistency with every other module directory's kebab-case naming. Because the module has never been committed or released, this is a plain rename with no downstream breakage to track — unlike a rename of an already-shipped module, there's no consumer pin or existing tag sequence to reconcile.

### Bug fixes prior to first release

- `outputs.tf`'s two outputs now reference `scaleway_iam_api_key.api_key` (matching `iam_policy.tf`'s actual resource label), and both gained `description` fields to match every other module's output style.
- `versions.tf` now pins `scaleway/scaleway ~> 2.0`, matching `scaleway/project/versions.tf` — no `hashicorp/random` pin is needed here, since unlike `object-bucket` this module never generates a randomized name suffix.
- `iam_policy.tf`'s `rule` block's `project_ids`/`permission_set_names` assignments are now alignment-formatted, matching this repo's `terraform fmt` convention.

### Required inputs: `name` and `permission_set_names`

- `name` drops its `default = null` and becomes required: it seeds both `"${var.name}-application"` and `"${var.name}-policy"`, so a `null` value would produce meaningless literal `"null-..."` resource names.
- `permission_set_names` drops its `default = null` and becomes required: a `rule` block with no permission sets grants nothing, so an invocation without it would silently produce an inert API key rather than failing loudly at plan time.
- `project_ids` stays optional (`default = null`): per `scaleway_iam_policy`'s own contract, omitting `project_ids` scopes the rule to the whole organization rather than specific projects — a legitimate use case (an account-wide policy), not a mistake to guard against.
- `description` stays optional (`default = null`): purely cosmetic, mirroring `scaleway/project`'s own `description` input.

### Release mechanics

This ships as the module's first release. Since it has never been tagged or referenced by a consumer, the required-input change is part of the initial interface, not a breaking change to something already released. `module-release.yml`'s discovery (`find digitalocean scaleway -mindepth 2 -maxdepth 2 -name versions.tf`, generalized in ADR-0006) picks up `scaleway/iam-policy` automatically once `versions.tf` is populated; no workflow-discovery change is needed. `module-docs.yml`'s hand-maintained `working-dir:` list and the root `README.md`'s Modules table both gain a `scaleway/iam-policy` entry, per the same manual-edit precedent established by every prior module addition.

## Consequences

- The module is usable end-to-end, producing a genuinely scoped IAM API key instead of one that would fail `terraform validate` or silently grant nothing.
- Consumers must supply `name` and `permission_set_names` up front; Terraform enforces this at plan time if either is omitted.
- `module-docs.yml`'s `working-dir:` list and the root `README.md` table both needed a manual edit, consistent with the existing pattern for every prior module addition.

## Out of scope

- Wiring `scaleway/iam-policy` into any `pigeon-do` stack.
- Exposing `scaleway_iam_application`'s own `id`/`name` as module outputs — only the API key's `access_key`/`secret_key` are exposed today.
- Renaming the `scw_access_key`/`scw_secret_key` outputs to match `digitalocean/access-key`'s unprefixed `access_key`/`secret_key` naming — the inconsistency is real but is an interface change with no forcing bug behind it, and there's no consumer yet to break by changing it later.
- Supporting more than one `rule` block per policy, or more than one policy per application.
- Updating `CLAUDE.md`'s ADR list — that list only references ADR-0001 and ADR-0006, the two ADRs that changed repo-wide scaffold/automation conventions. This ADR is a single-module interface/naming decision, the same category as ADRs 0002–0005, 0007, and 0008, none of which are referenced there either.
