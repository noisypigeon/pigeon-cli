# ADR-0059: multiple root directories

- **Author**: Willow Finch ([@noisypigeon](https://github.com/noisypigeon)).
- **Date**: 2026-09-24.
- **Status**: Accepted.
- **Origin**: `pigeon-do` ADR-0007.

## Context

`pigeon-do` currently manages exactly one DigitalOcean account/domain, `pigeon.dev`, and `root.hcl` is written accordingly: it lives at the true repo root (one level above `pigeon.dev/`), reads a single repo-root `.env` (`root_env_path = "${get_repo_root()}/.env"`), injects a single repo-root `env.tf`, and its `provider`/`remote_state` blocks hardcode one `DIGITALOCEAN_TOKEN`/Spaces-key pair, one state bucket (`DIGITALOCEAN_TERRAFORM_STATE_BUCKET`), and one region (`tor1`). ADR-0053 made this a deliberate, explicitly provisional simplification:

> Unlike `topology-v1`, `pigeon-do` only scaffolds a single domain/DO team (`pigeon.dev`), not two — so the secrets model below is simpler than topology-v1's root+tree override chain. [...] No tree-level `.env` override chain (unlike `topology-v1`) — with a single domain/team, one root `.env` is sufficient. **If a second domain or DO team is added later, revisit this decision rather than retrofitting it silently.**

That trigger has arrived: `noisypigeon.com` is being added as a second DigitalOcean account, entirely separate from `pigeon.dev` — its own API token, its own Spaces access key/secret, and its own Terraform state bucket, not shared with `pigeon.dev` in any way. A single `root.hcl`/`.env` pair cannot represent two accounts' credentials and backends at once, so `root.hcl`'s account-specific pieces need to become per-account rather than per-repo.

## Decision

### Root directories

Introduce the concept of a **root directory**: one top-level directory per DigitalOcean account/domain, self-contained with its own `root.hcl`, `.env` (git-ignored), `.env.example` (tracked), and `env.tf`. `pigeon.dev/` becomes the first root directory — today's repo-root `root.hcl`, `.env`, `.env.example`, and `env.tf` move down into it unchanged in content (aside from the path-lookup change below). `noisypigeon.com/` is added as a second, sibling root directory with the same shape:

```
pigeon-do/
  common.hcl
  pigeon.dev/
    root.hcl
    .env               # git-ignored
    .env.example
    env.tf
    cloudflare/...
    digitalocean/...
  noisypigeon.com/
    root.hcl
    .env               # git-ignored
    .env.example
    env.tf
    digitalocean/...
```

Each root directory is discovered the same way `root.hcl` is discovered today: every leaf's `terragrunt.hcl` already does `include "root" { path = find_in_parent_folders("root.hcl") }`, which walks upward from the leaf and stops at the *nearest* `root.hcl`. Once each account has its own `root.hcl`, this same mechanism naturally isolates accounts — a leaf under `pigeon.dev/` will never resolve `noisypigeon.com/root.hcl` or vice versa. The only leaf-level change is a second, identical `include "common"` block (see "Shared `common.hcl`" below) — nothing account-specific needs to be added per leaf.

### Locating each account's own `.env`/`env.tf`

`root.hcl`'s current lookups assume the true repo root:

```hcl
root_env_path = "${get_repo_root()}/.env"
...
generate "env" {
  contents = file("${get_repo_root()}/env.tf")
}
```

`get_repo_root()` always returns the true git root regardless of which account subtree is being evaluated, so it can no longer be used for account-scoped files. Both lookups switch to `find_in_parent_folders`, the same "search upward from the leaf, stop at the nearest match" primitive already used for `root.hcl` itself:

```hcl
root_env_path = find_in_parent_folders(".env", "")
...
generate "env" {
  contents = file(find_in_parent_folders("env.tf"))
}
```

This resolves to whichever account's `.env`/`env.tf` is nearest the leaf being evaluated, with no hardcoded path and no per-account branching logic. The `env_ancestors` mechanism (ADR-0053) keeps working as-is, except its de-duplication filter — which currently excludes the repo-root `env.tf` already injected by `generate "env"` (`abspath(p) != abspath("${get_repo_root()}/env.tf")`) — compares against the account root's `env.tf` (the same path `find_in_parent_folders("env.tf")` resolves) instead.

### Shared `common.hcl`

Not everything in today's `root.hcl` is account-specific. `pigeon_tf_root` (ADR-0054) points at a sibling checkout of the whole `pigeon-tf` repo — a repo-wide concern, identical for every account, and already correctly rooted at `get_repo_root()`:

```hcl
pigeon_tf_root = get_env("PIGEON_TF_PATH", "${dirname(get_repo_root())}/pigeon-tf")
```

A new repo-root `common.hcl` holds this local and its `generate "pigeon_tf"` block. **Confirmed empirically during implementation**: Terragrunt does not support nested includes ("`root.hcl` includes `common.hcl`" fails with *"Only one level of includes is allowed"*), so each leaf's `terragrunt.hcl` includes `common.hcl` directly, alongside its existing `include "root"`:

```hcl
include "root" {
  path = find_in_parent_folders("root.hcl")
}

include "common" {
  path = find_in_parent_folders("common.hcl")
}
```

This also ruled out putting `cloudflare_ids`/`bucket_names` in `common.hcl` as originally intended here: those `generate` blocks (and the `bucket_name_secrets` local behind the latter) depend on `local.secrets`, and Terragrunt evaluates each `include`'s `locals` block independently — a local in `common.hcl` can't forward-reference a local defined only in `root.hcl`'s own `locals` block (confirmed by a real `terragrunt plan` failure: *"The local reference 'secrets' is not evaluated"*). So `cloudflare_ids`/`bucket_names` stay in each account's own `root.hcl`, next to the `secrets` local they depend on — duplicated per account, small as that duplication is.

What stays in each account's own `root.hcl`: the `secrets`/`root_secrets` locals (now via `find_in_parent_folders`, above), `cloudflare_ids`/`bucket_names`, the `provider "digitalocean"`/`provider "cloudflare"` generate block, and the `remote_state` block.

### Preserving existing state keys

`remote_state.config.key` was `"${path_relative_to_include()}/terraform.tfstate"`, relative to wherever `root.hcl` is found. Moving `root.hcl` into `pigeon.dev/` shortens that relative path for every existing leaf (it no longer includes the `pigeon.dev/` segment), which Terraform sees as a backend configuration change requiring state migration — **confirmed empirically**: `terraform init` on an existing leaf refused to proceed with *"Backend configuration changed"* until this was fixed. The fix is to prefix the key with the account directory name explicitly, restoring the exact original key so no migration is needed:

```hcl
key = "pigeon.dev/${path_relative_to_include()}/terraform.tfstate"
```

`noisypigeon.com/root.hcl` uses the equivalent `"noisypigeon.com/${path_relative_to_include()}/terraform.tfstate"` — not required for correctness (it has its own bucket, so no collision risk either way), but kept for consistency. Verified with a real `terragrunt plan` against every existing `pigeon.dev` leaf post-migration: all seven report "No changes. Your infrastructure matches the configuration."

### Adding `noisypigeon.com`

`noisypigeon.com/root.hcl` is a new file following the same shape as `pigeon.dev/root.hcl`, with its own `remote_state.config.bucket` (its own `DIGITALOCEAN_TERRAFORM_STATE_BUCKET` key, read from `noisypigeon.com/.env`) and `region`/`endpoint` defaulting to `tor1`, same as `pigeon.dev` — a plain value in that file, so a future account can pick a different region without touching anything else. `noisypigeon.com/.env.example` documents the same key shape as `pigeon.dev/.env.example` (`DIGITALOCEAN_TOKEN`, `DIGITALOCEAN_SPACES_ACCESS_ID`, `DIGITALOCEAN_SPACES_SECRET_KEY`, `DIGITALOCEAN_TERRAFORM_STATE_BUCKET`, plus any `BUCKET_NAME_*`/Cloudflare keys once `noisypigeon.com` needs them); the real `noisypigeon.com/.env` values (a fresh API token and Spaces key created in the *separate* `noisypigeon.com` DigitalOcean account) are filled in out-of-band by whoever has access to that account, never generated or guessed.

`noisypigeon.com`'s first leaf will be its own `terraform-state` bucket, hitting the exact bootstrap-ordering problem ADR-0055 solved for `pigeon.dev`. That leaf reuses ADR-0055's runbook unchanged (temporary `remote_state { backend = "local" }` override, apply, capture the bucket name into `noisypigeon.com/.env`, revert, `terragrunt init -migrate-state`) — this ADR does not redesign that procedure, only notes it repeats per root directory.

### `.gitignore`

No change needed. `.gitignore`'s `.env` entry has no leading `/`, so it already matches a file named `.env` at any depth — `pigeon.dev/.env` and `noisypigeon.com/.env` are both already excluded.

### `mise.toml` tasks

No change needed. `mise run plan`/`mise run apply` (`terragrunt run --all -- plan`/`apply`) discover every leaf's *nearest* `root.hcl` independently — that is already how the single-root case works — so once each account subtree has its own `root.hcl`, a single invocation from the repo root transparently fans out across every root directory, each leaf resolving its own account's credentials and backend. A per-root-directory scoped task (e.g. planning/applying just one account at a time for faster iteration) is a reasonable future enhancement but not required for correctness, so it's left out of scope here.

## Consequences

- `pigeon-do` can host any number of fully-isolated DigitalOcean accounts, each with its own credentials and state bucket, by adding a new root directory plus one `include "common"` block per leaf.
- `pigeon.dev/root.hcl`'s content is unchanged in substance; only its location, the `get_repo_root()` → `find_in_parent_folders()` path lookups, and the `remote_state.config.key` prefix change.
- Every existing leaf's `terragrunt.hcl` gained one small, identical `include "common" { path = find_in_parent_folders("common.hcl") }` block — the only leaf-level change this ADR required, and purely mechanical.
- `root.hcl` for a new account is still a real file a human writes (secrets loading, `cloudflare_ids`/`bucket_names`, provider, remote_state) — this ADR does not generate `root.hcl` from a template. `common.hcl` only holds the one local (`pigeon_tf_root`) that's genuinely independent of `local.secrets`.
- One extra layer of indirection (`common.hcl` include, now per-leaf rather than nested through `root.hcl`) to read when tracing how a leaf resolves `local.pigeon_tf_root`.

## Out of scope

- Provisioning any real `noisypigeon.com` DigitalOcean or Cloudflare resources (management project, terraform-state bucket, etc.) — follow-up work once `noisypigeon.com/.env` has real credentials, per ADR-0055's pattern. `noisypigeon.com/root.hcl`/`.env.example`/`env.tf` exist; `noisypigeon.com/.env` does not, since real credentials aren't generated or guessed.
- Adding Cloudflare/`BUCKET_NAME_*` keys to `noisypigeon.com` — its `root.hcl` has no `cloudflare_ids`/`bucket_names` blocks yet; add them following `pigeon.dev/root.hcl`'s pattern when needed.
- Per-root-directory scoped `mise` tasks (e.g. `mise run plan pigeon.dev`) — the existing whole-repo tasks already work correctly across multiple root directories; scoping is a convenience, not a requirement.
- Sharing any resource (project, bucket, DNS zone) between `pigeon.dev` and `noisypigeon.com` — they are treated as fully independent accounts with no cross-account references.
