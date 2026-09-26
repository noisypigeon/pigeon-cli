# Terraform modules

Versioned, reusable Terraform modules, consumed by [`pigeon-do`](https://github.com/noisypigeon/pigeon-do) and any future infra repos. Originally the standalone `pigeon-tf` repo, merged into `pigeon-cli` by [ADR-0037](../docs/adr/0037-merge-pigeon-tf-terraform-modules.md); see `pigeon-do`'s [ADR-0002](https://github.com/noisypigeon/pigeon-do/blob/main/docs/adr/0002-pigeon-tf-scaffold.md) for the design decisions behind the original repo.

This directory holds only module source — it has no root provider/backend configuration and is never `terraform`/`terragrunt` run standalone.

## Modules

| Path | Description |
| --- | --- |
| `modules/digitalocean/access-key` | A DigitalOcean Spaces access key (`digitalocean_spaces_key`), optionally scoped to one or more buckets. |
| `modules/digitalocean/standard-storage-bucket` | A DigitalOcean Spaces bucket (`digitalocean_spaces_bucket`) with a randomized name suffix. |
| `modules/digitalocean/cold-storage-bucket` | A data-source wrapper for a DigitalOcean Spaces Cold Storage bucket (not yet supported as a Terraform resource by the DO provider — the bucket is created click-ops and managed as a data source), optionally attached to a project. |
| `modules/digitalocean/project` | A thin wrapper around `digitalocean_project`. |
| `modules/digitalocean/droplet` | A DigitalOcean droplet (`digitalocean_droplet`) with cloud-init provisioning (rclone, an LVM auto-combine script for attached volumes, a sudo user) and a Cloudflare DNS alias. |
| `modules/digitalocean/block-storage-volume` | One or more DigitalOcean Block Storage volumes (`digitalocean_volume`), attached to a droplet (`digitalocean_volume_attachment`). |
| `modules/scaleway/project` | A thin wrapper around `scaleway_account_project`. |
| `modules/scaleway/object-bucket` | A Scaleway Object Storage bucket (`scaleway_object_bucket`) with a randomized name suffix, versioning, and a standard/glacier storage-class toggle implemented via an immediate lifecycle transition. |
| `modules/scaleway/iam-policy` | A Scaleway `scaleway_iam_application` and `scaleway_iam_policy` wrapper to produce a restricted `scaleway_iam_api_key` using permission sets. |

## Versioning

Releases are tagged on `pigeon-cli`'s `main` with per-module, path-scoped semantic versions (`terraform/modules/<provider>/<module>/vX.Y.Z`). Consuming repos pin to a tag by checking out that tag in their local clone of this repo. Tags created before the ADR-0037 merge keep their original, shorter form (`v0.1.0`-`v0.1.3` repo-wide, `<provider>/<module>/vX.Y.Z` per-module) — see ADR-0037 for why they weren't renamed.

## Consuming locally

Since consumers run Terragrunt/Terraform locally (no remote module source), clone this repo as a sibling directory to the consuming repo and check out the tag you want:

```
git clone git@github.com:noisypigeon/pigeon-cli.git ../pigeon-cli
cd ../pigeon-cli && git checkout terraform/modules/digitalocean/access-key/v0.1.0
```

then reference modules by their path under `terraform/modules/`, e.g.
`../pigeon-cli/terraform/modules/digitalocean/access-key`.
