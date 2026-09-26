locals {
  # Secrets: read from the shared repo-root ".env" file, found by walking up
  # from this leaf's directory — see docs/adr/0011-shared-root-env-and-cloudflare-migration.md.
  # This file manages BOTH noisypigeon.com's and pigeon.dev's Cloudflare
  # leaves (two different Cloudflare accounts) — see
  # docs/adr/0012-merge-pigeon-dev-into-cloudflare-root.md. Which account's
  # credentials apply is resolved per-leaf below, by directory path.
  root_env_path = find_in_parent_folders(".env", "")

  root_secrets = { for pair in [
    for line in split("\n", fileexists(local.root_env_path) ? file(local.root_env_path) : "") :
    regex("^([^=]+)=(.*)$", trimspace(line))
    if trimspace(line) != "" && !startswith(trimspace(line), "#")
    && can(regex("^([^=]+)=(.*)$", trimspace(line)))
  ] : trimspace(pair[0]) => trimspace(pair[1]) }

  secrets = local.root_secrets

  # pigeon.dev and noisypigeon.com are different Cloudflare accounts — a
  # leaf under global/pigeon.dev/ gets pigeon.dev's token/account id,
  # every other leaf gets noisypigeon.com's. See ADR-0012.
  is_pigeon_dev_leaf = startswith(path_relative_to_include(), "global/pigeon.dev/")

  cloudflare_api_token  = local.is_pigeon_dev_leaf ? get_env("CLOUDFLARE_PIGEON_DEV_TOKEN", lookup(local.secrets, "CLOUDFLARE_PIGEON_DEV_TOKEN", "")) : get_env("CLOUDFLARE_NOISYPIGEON_COM_TOKEN", lookup(local.secrets, "CLOUDFLARE_NOISYPIGEON_COM_TOKEN", ""))
  cloudflare_account_id = local.is_pigeon_dev_leaf ? get_env("CLOUDFLARE_PIGEON_DEV_ACCOUNT_ID", lookup(local.secrets, "CLOUDFLARE_PIGEON_DEV_ACCOUNT_ID", "")) : get_env("CLOUDFLARE_NOISYPIGEON_COM_ACCOUNT_ID", lookup(local.secrets, "CLOUDFLARE_NOISYPIGEON_COM_ACCOUNT_ID", ""))
}

generate "cloudflare_ids" {
  path      = "cloudflare_ids_generated.tf"
  if_exists = "overwrite"
  contents  = <<EOF
locals {
  cloudflare_account_id              = "${local.cloudflare_account_id}"
  cloudflare_noisypigeon_com_zone_id = "${get_env("CLOUDFLARE_NOISYPIGEON_COM_ZONE_ID", lookup(local.secrets, "CLOUDFLARE_NOISYPIGEON_COM_ZONE_ID", ""))}"
  cloudflare_pigeon_dev_zone_id      = "${get_env("CLOUDFLARE_PIGEON_DEV_ZONE_ID", lookup(local.secrets, "CLOUDFLARE_PIGEON_DEV_ZONE_ID", ""))}"
}
EOF
}

generate "provider" {
  path      = "provider_generated.tf"
  if_exists = "overwrite"
  contents  = <<EOF
terraform {
  required_providers {
    cloudflare = {
      source  = "cloudflare/cloudflare"
      version = "~> 5"
    }
  }
}

provider "cloudflare" {
  api_token         = "${local.cloudflare_api_token}"
}
EOF
}

# Configure backend to use Scaleway's own Object Storage bucket, not the
# DigitalOcean Spaces bucket — see docs/adr/0011-shared-root-env-and-cloudflare-migration.md.
remote_state {
  backend = "s3"

  config = {
    endpoints = {
      s3 = "https://s3.fr-par.scw.cloud"
    }
    region = "fr-par"
    bucket = lookup(local.secrets, "SCALEWAY_TERRAFORM_STATE_BUCKET_NAME", "")
    # Prefixed with "cloudflare" so leaves' state keys are unchanged now
    # that this file lives inside cloudflare/ instead of the repo root —
    # see docs/adr/0009-per-provider-root-hcl.md.
    key                         = "cloudflare/${path_relative_to_include()}/terraform.tfstate"
    skip_credentials_validation = true
    skip_metadata_api_check     = true
    skip_region_validation      = true
    skip_requesting_account_id  = true

    access_key = get_env("SCALEWAY_ACCESS_KEY", lookup(local.secrets, "SCALEWAY_ACCESS_KEY", ""))
    secret_key = get_env("SCALEWAY_SECRET_KEY", lookup(local.secrets, "SCALEWAY_SECRET_KEY", ""))
  }

  generate = {
    path      = "backend.tf"
    if_exists = "overwrite"
  }
}
