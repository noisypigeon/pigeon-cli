# pigeon-tf

Versioned, reusable Terraform modules, consumed by [`pigeon-do`](https://github.com/noisypigeon/pigeon-do) and any future infra repos. See `pigeon-do`'s [ADR-0002](https://github.com/noisypigeon/pigeon-do/blob/main/docs/adr/0002-pigeon-tf-scaffold.md) for the design decisions behind this repo.

This repo holds only module source — it has no root provider/backend configuration and is never `terraform`/`terragrunt` run standalone.

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

Releases are tagged on `main` with semantic versions (`vX.Y.Z`). Consuming repos pin to a tag by checking out that tag in their local clone of this repo — there's no registry or CI-driven release process (yet).

## Consuming locally

Since consumers run Terragrunt/Terraform locally (no remote module source), clone this repo as a sibling directory to the consuming repo and check out the tag you want:

```
git clone git@github.com:noisypigeon/pigeon-tf.git ../pigeon-tf
cd ../pigeon-tf && git checkout v0.1.0
```
