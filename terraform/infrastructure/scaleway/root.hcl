locals {
  # Secrets: read from this provider's own ".env" file (scaleway/.env),
  # found the same way every leaf finds this root.hcl itself (nearest
  # ancestor). Backend credentials (SCALEWAY_ACCESS_KEY/SCALEWAY_SECRET_KEY)
  # are the same ones used by the provider block below — scaleway/* has its
  # own dedicated state bucket, not the digitalocean/cloudflare shared one —
  # see docs/adr/0010-scaleway-remote-state.md.
  root_env_path = find_in_parent_folders(".env", "")

  root_secrets = { for pair in [
    for line in split("\n", fileexists(local.root_env_path) ? file(local.root_env_path) : "") :
    regex("^([^=]+)=(.*)$", trimspace(line))
    if trimspace(line) != "" && !startswith(trimspace(line), "#")
    && can(regex("^([^=]+)=(.*)$", trimspace(line)))
  ] : trimspace(pair[0]) => trimspace(pair[1]) }

  secrets = local.root_secrets
}

generate "provider" {
  path      = "provider_generated.tf"
  if_exists = "overwrite"
  contents  = <<EOF
terraform {
  required_providers {
    scaleway = {
      source  = "scaleway/scaleway"
      version = "~> 2.0"
    }
  }
}

provider "scaleway" {
  access_key      = "${get_env("SCALEWAY_ACCESS_KEY", lookup(local.secrets, "SCALEWAY_ACCESS_KEY", ""))}"
  secret_key      = "${get_env("SCALEWAY_SECRET_KEY", lookup(local.secrets, "SCALEWAY_SECRET_KEY", ""))}"
  organization_id = "${get_env("SCALEWAY_ORGANIZATION_ID", lookup(local.secrets, "SCALEWAY_ORGANIZATION_ID", ""))}"
  zone   = "fr-par-1"
  region = "fr-par"
}
EOF
}

generate "scaleway_ids" {
  path      = "scaleway_ids_generated.tf"
  if_exists = "overwrite"
  contents  = <<EOF
locals {
  scaleway_organization_id            = "${get_env("SCALEWAY_ORGANIZATION_ID", lookup(local.secrets, "SCALEWAY_ORGANIZATION_ID", ""))}"
  scaleway_project_id_noisypigeon_com = "${get_env("SCALEWAY_PROJECT_ID_NOISYPIGEON_COM", lookup(local.secrets, "SCALEWAY_PROJECT_ID_NOISYPIGEON_COM", ""))}"
  scaleway_project_id_pigeon_dev      = "${get_env("SCALEWAY_PROJECT_ID_PIGEON_DEV", lookup(local.secrets, "SCALEWAY_PROJECT_ID_PIGEON_DEV", ""))}"
}
EOF
}

# Configure backend to use Scaleway's own Object Storage bucket, not the
# digitalocean/cloudflare shared Spaces bucket — see
# docs/adr/0010-scaleway-remote-state.md.
remote_state {
  backend = "s3"

  config = {
    endpoints = {
      s3 = "https://s3.fr-par.scw.cloud"
    }
    region                      = "fr-par"
    bucket                      = lookup(local.secrets, "SCALEWAY_TERRAFORM_STATE_BUCKET_NAME", "")
    key                         = "scaleway/${path_relative_to_include()}/terraform.tfstate"
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
