# Changelog

All notable changes to this module are documented in this file.

The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

## [1.0.0] - 2026-09-25

### Split scaleway/iam-policy permission grants by scope

**Breaking**: replaces the single `permission_set_names` input with two separate inputs, `org_permission_sets` and `project_permission_sets`, each granted through its own `scaleway_iam_policy` rule block. Scaleway IAM permission sets aren't uniformly project-scoped — grants like `IAMManager` or `ProjectManager` are organization-level, while grants like `InstancesFullAccess` are meant to be scoped to specific projects — and a single rule/`project_ids` pair couldn't represent both. A new `organization_id` input scopes the org-level rule; the existing `project_ids` input continues to scope the project-level rule. All four scoping/grant inputs (`organization_id`, `org_permission_sets`, `project_ids`, `project_permission_sets`) are optional, but since both rule blocks are always created, a consumer generally needs to populate whichever pairing it cares about.

**Breaking**: renames the module's outputs from `scw_access_key`/`scw_secret_key` to `access_key`/`secret_key`, matching `digitalocean/access-key`'s unprefixed naming convention.

See ADR-0010 for the full reasoning, including why this diverges from ADR-0008's "no consumers yet" precedent — this module already has a real consumer (`pigeon-do`'s `terraform-deployer` leaf), which needs a coordinated follow-up update (bumping its pinned `ref` and updating its output references) once this release lands.

[#16](https://github.com/noisypigeon/pigeon-tf/pull/16)

## [0.1.0] - 2026-09-25

### Add scaleway/iam-policy module

Adds `scaleway/iam-policy`, a module wrapping `scaleway_iam_application`, `scaleway_iam_policy`, and `scaleway_iam_api_key` to produce a permission-scoped Scaleway API key. It creates an IAM application, attaches a policy rule granting a set of `permission_set_names` (optionally restricted to `project_ids`), and mints an API key against that application — useful for handing automation (e.g. Terraform itself) a credential limited to exactly the permissions it needs.

`name` and `permission_set_names` are required inputs: `name` seeds both the application's and policy's resource names, and a policy with no `permission_set_names` would silently grant nothing. `project_ids` and `description` are optional — omitting `project_ids` scopes the policy to the whole organization rather than specific projects, which is a legitimate use case.

See ADR-0009 for the full set of decisions, including why the module directory is `iam-policy` (kebab-case, matching every other module in this repo) rather than the underscore form it was initially scaffolded with.

[#15](https://github.com/noisypigeon/pigeon-tf/pull/15)
