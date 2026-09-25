# Changelog

All notable changes to this module are documented in this file.

The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

## [0.1.0] - 2026-09-25

### Add scaleway/iam-policy module

Adds `scaleway/iam-policy`, a module wrapping `scaleway_iam_application`, `scaleway_iam_policy`, and `scaleway_iam_api_key` to produce a permission-scoped Scaleway API key. It creates an IAM application, attaches a policy rule granting a set of `permission_set_names` (optionally restricted to `project_ids`), and mints an API key against that application — useful for handing automation (e.g. Terraform itself) a credential limited to exactly the permissions it needs.

`name` and `permission_set_names` are required inputs: `name` seeds both the application's and policy's resource names, and a policy with no `permission_set_names` would silently grant nothing. `project_ids` and `description` are optional — omitting `project_ids` scopes the policy to the whole organization rather than specific projects, which is a legitimate use case.

See ADR-0009 for the full set of decisions, including why the module directory is `iam-policy` (kebab-case, matching every other module in this repo) rather than the underscore form it was initially scaffolded with.

[#15](https://github.com/noisypigeon/pigeon-tf/pull/15)
