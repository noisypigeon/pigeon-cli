# Terraform modules

Versioned, reusable Terraform modules, consumed by this repo's own
[`terraform/infrastructure/`](../infrastructure/) and any future infra
repos. Originally the standalone `pigeon-tf` repo, merged into this repo by
[ADR-0037](../../docs/adr/0037-merge-pigeon-tf-terraform-modules.md); see
[ADR-0054](../../docs/adr/0054-pigeon-tf-scaffold.md) (originally
`pigeon-do` ADR-0002) for the design decisions behind consuming the
original repo.

This directory holds only module source — it has no root provider/backend configuration and is never `terraform`/`terragrunt` run standalone.

## Modules

| Path | Description |
| --- | --- |
| `digitalocean/access-key` | A DigitalOcean Spaces access key (`digitalocean_spaces_key`), optionally scoped to one or more buckets. |
| `digitalocean/standard-storage-bucket` | A DigitalOcean Spaces bucket (`digitalocean_spaces_bucket`) with a randomized name suffix. |
| `digitalocean/cold-storage-bucket` | A data-source wrapper for a DigitalOcean Spaces Cold Storage bucket (not yet supported as a Terraform resource by the DO provider — the bucket is created click-ops and managed as a data source), optionally attached to a project. |
| `digitalocean/project` | A thin wrapper around `digitalocean_project`. |
| `digitalocean/droplet` | A DigitalOcean droplet (`digitalocean_droplet`) with cloud-init provisioning (rclone, an LVM auto-combine script for attached volumes, a sudo user) and a Cloudflare DNS alias. |
| `digitalocean/block-storage-volume` | One or more DigitalOcean Block Storage volumes (`digitalocean_volume`), attached to a droplet (`digitalocean_volume_attachment`). |
| `scaleway/project` | A thin wrapper around `scaleway_account_project`. |
| `scaleway/object-bucket` | A Scaleway Object Storage bucket (`scaleway_object_bucket`) with a randomized name suffix, versioning, and a standard/glacier storage-class toggle implemented via an immediate lifecycle transition. |
| `scaleway/iam-policy` | A Scaleway `scaleway_iam_application` and `scaleway_iam_policy` wrapper to produce a restricted `scaleway_iam_api_key` using permission sets. |

## Versioning

Releases are tagged on `pigeon`'s `main` with per-module, path-scoped semantic versions (`terraform/modules/<provider>/<module>/vX.Y.Z`). Consuming repos pin to a tag by checking out that tag in their local clone of this repo. Tags created before the ADR-0037 merge keep their original, shorter form (`v0.1.0`-`v0.1.3` repo-wide, `<provider>/<module>/vX.Y.Z` per-module) — see ADR-0037 for why they weren't renamed.

## Consuming

This repo's own [`terraform/infrastructure/`](../infrastructure/) consumes
these modules directly, in the same working tree, via a tagged `git::`
source pointing back at this same repo — e.g.:

```hcl
module "state_bucket" {
  source = "git::https://github.com/noisypigeon/pigeon.git//terraform/modules/scaleway/object-bucket?ref=scaleway/object-bucket/v0.1.0"
  ...
}
```

An *external* infra repo (no local checkout of this repo) consumes modules
the same way — a tagged `git::` source works identically for any consumer,
in-repo or not. For local iteration against an unreleased module change
(no network fetch), clone this repo as a sibling directory instead and
reference it by path:

```
git clone git@github.com:noisypigeon/pigeon.git ../pigeon
```

then reference modules under `terraform/modules/`, e.g.
`../pigeon/terraform/modules/digitalocean/access-key`.
