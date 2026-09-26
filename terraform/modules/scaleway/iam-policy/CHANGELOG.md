# Changelog

All notable changes to this module are documented in this file.

The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

## [3.0.1] - 2026-09-26

### Fix invalid variable validation in scaleway/iam-policy

The v3.0.0 cross-variable `validation` block on `variable "name"` (added in ADR-0012) never referenced `var.name` itself — it only checked `organization_id`/`project_ids`/`bucket_names`. OpenTofu requires a variable's validation condition to genuinely reference that variable's own value, and rejects one that doesn't at `init`/`plan` time with `Invalid variable validation condition`, breaking every consumer of the module.

Fixes this by adding a real `var.name != ""` check alongside the existing cross-variable logic, preserving the intended behavior (at least one of `organization_id`+`organization_permission_sets`, `project_ids`+`project_permission_sets`, or `bucket_names`+`bucket_actions` must be fully set) while satisfying OpenTofu's requirement. No interface change.

[#20](https://github.com/noisypigeon/pigeon-tf/pull/20)

## [3.0.0] - 2026-09-26

### Make scaleway/iam-policy scoping mechanisms independently optional

**Breaking**: changes `bucket_names` from `list(string)` to `map(string)` (caller supplies a static logical key per bucket, e.g. `{ email = module.data_email.name }`). This fixes a real `plan`-time crash: `bucket_names` fed straight into a `for_each = toset(var.bucket_names)` failed whenever a bucket name contained an apply-time-only value (e.g. a `random_string` suffix from `scaleway/object-bucket`), because `for_each` over a set requires every element to be known at plan time. With a map, only the caller-chosen key needs to be known up front — the bucket name value itself can remain unknown until apply.

Makes the underlying `scaleway_iam_policy` resource and its `rule` blocks conditional: a `rule` (and the policy resource itself) is only created when its corresponding scope is *fully* populated (both the id/list field and a non-empty permission-set list). Previously both rule blocks were created unconditionally, which meant a consumer wanting only bucket-scoped access — leaving `organization_id`/`project_ids` unset — would fail at apply time, since Scaleway requires each `rule` to set one or the other. A `moved` block preserves already-applied state for existing consumers that fully populate both org and project scopes.

Adds a cross-variable `validation` requiring at least one of `organization_id`+`organization_permission_sets`, `project_ids`+`project_permission_sets`, or `bucket_names`+`bucket_actions` be fully set — so a config granting nothing now fails fast with a clear message. This depends on Terraform/OpenTofu 1.9+ variable-validation cross-references, so `versions.tf` now pins `required_version = ">= 1.9.0"`.

See ADR-0012 for the full reasoning, including why an org/project rule with a set id but an empty permission-set list no longer produces a rule at all (superseding an untested assumption from ADR-0010).

[#19](https://github.com/noisypigeon/pigeon-tf/pull/19)

## [2.0.0] - 2026-09-26

### Add bucket resource-scoping to scaleway/iam-policy

**Breaking**: renames the `org_permission_sets` input to `organization_permission_sets`, for consistency with this module's other already-full-length names (`organization_id`, `project_permission_sets`, `project_ids`).

Adds new `bucket_names` and `bucket_actions` inputs, wired into a new `scaleway_object_bucket_policy` resource created once per name in `bucket_names`, granting the module's IAM application access to exactly those Object Storage buckets. Both inputs default to an empty list, so no bucket access is granted unless a consumer explicitly lists both which buckets and which actions it wants — this module never derives a default action bundle just because a bucket name was supplied. `bucket_actions` is validated against a known set of S3 actions (`s3:ListBucket`, `s3:GetObject`, `s3:PutObject`, `s3:DeleteObject`) to catch typos.

Note for anyone reaching for "wildcard" bucket access (e.g. "all buckets prefixed `data-`"): Scaleway has no bucket-listing data source and no cross-bucket wildcard in its bucket-policy `Resource` field, so this module only accepts exact bucket names — resolve any prefix-matching in your own Terraform (e.g. `[for b in local.known_buckets : b if startswith(b, "data-")]`) before passing the list in. Also note that granting a blanket `ObjectStorage*` permission set via `organization_permission_sets`/`project_permission_sets` already grants access to every bucket in scope, since Scaleway policy rules are allow-only — `bucket_names` can't narrow that back down, so real per-bucket least privilege means not also granting a blanket Object Storage permission set in the same policy.

See ADR-0011 for the full reasoning, including the Scaleway API constraints that ruled out an IAM-native resource-condition approach.

[#18](https://github.com/noisypigeon/pigeon-tf/pull/18)

## [1.1.0] - 2026-09-25

### Add expires_at input to scaleway/iam-policy

Adds an optional `expires_at` input, wired into the `scaleway_iam_api_key` resource's own `expires_at` argument, letting a consumer set an expiration timestamp (e.g. `2027-09-25T22:32:12Z`) on the minted API key. Defaults to `null` (no expiration), so existing consumers are unaffected.

[#17](https://github.com/noisypigeon/pigeon-tf/pull/17)

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
