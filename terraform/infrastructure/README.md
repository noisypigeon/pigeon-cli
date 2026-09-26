# Terraform infrastructure

Live Terragrunt/Terraform configuration for personal infrastructure across
two domains/accounts — `noisypigeon.com` and `pigeon.dev` — spanning
DigitalOcean, Cloudflare, and Scaleway. Originally the standalone
`pigeon-do` repo, merged into this repo by
[ADR-0052](../../docs/adr/0052-merge-pigeon-do-terraform-infrastructure.md);
see [`docs/adr/0053`-`0064`](../../docs/adr/) (each carrying an `Origin:
pigeon-do ADR-000N` bullet) for the design decisions behind the original
repo, including how its layout arrived at what's actually on disk today.

This directory consumes [`terraform/modules/`](../modules/) — never a local
path, always a tagged `git::` source, the same way any other consumer
would (see [`terraform/modules/README.md`](../modules/README.md)):

```hcl
module "bucket" {
  source            = "git::https://github.com/noisypigeon/pigeon.git//terraform/modules/scaleway/object-bucket?ref=scaleway/object-bucket/v0.1.0"
  enable_versioning = true
  namespace         = "terraform"
  name              = "state"
}
```

## Structure

Provider-rooted, per [ADR-0061](../../docs/adr/0061-per-provider-root-hcl.md):

```
cloudflare/
  root.hcl                              # secrets, cloudflare_ids, provider, remote_state
  global/<domain>/<leaf>/               # e.g. global/noisypigeon.com/fastmail
digitalocean/
  root.hcl                              # secrets, bucket_names, do_projects, provider, remote_state
  env.tf
  <region>/<domain>/<category>/<leaf>/  # e.g. tor1/noisypigeon.com/data/backblaze-import
scaleway/
  root.hcl                              # secrets, provider, remote_state
  <region-or-global>/<domain>/<leaf>/   # e.g. fr-par/noisypigeon.com/terraform
```

Every leaf's `terragrunt.hcl` is the same shape:

```hcl
include "root" {
  path = find_in_parent_folders("root.hcl")
}

terraform {
  source = get_terragrunt_dir()
}
```

`root.hcl` is found by walking up from the leaf to the nearest match, so
each provider's leaves automatically resolve that provider's own
`root.hcl` — no per-leaf provider selection needed.

## Secrets

A single root `.env` (git-ignored, never committed) and `.env.example`
(tracked, blank), per
[ADR-0063](../../docs/adr/0063-shared-root-env-and-cloudflare-migration.md).
Every provider's `root.hcl` reads it via `find_in_parent_folders(".env",
"")`, walking up from wherever that `root.hcl` lives to this directory's
root. A real shell environment variable always overrides the `.env` file
value for the same key.

## Getting started

```sh
cp .env.example .env   # then fill in real values — never commit this file
```

`terraform`/`terragrunt` aren't yet wired into this repo's root
`.mise.toml` (a proposed follow-up, not done as of this writing) — run
Terragrunt directly, per leaf:

```sh
cd cloudflare/global/noisypigeon.com/fastmail
terragrunt plan
terragrunt apply
```

or across every leaf under one provider at once:

```sh
cd digitalocean
terragrunt run --all -- plan
```
