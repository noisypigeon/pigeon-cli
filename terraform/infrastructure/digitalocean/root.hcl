locals {
  # Secrets: read from this provider's own ".env" file, found the same way
  # every leaf finds this root.hcl itself (nearest ancestor). Backend
  # credentials (DIGITALOCEAN_SPACES_*/TERRAFORM_STATE_BUCKET) are
  # duplicated into cloudflare/.env and scaleway/.env too, since all three
  # providers share the same state bucket — see
  # docs/adr/0009-per-provider-root-hcl.md.
  root_env_path = find_in_parent_folders(".env", "")

  root_secrets = { for pair in [
    for line in split("\n", fileexists(local.root_env_path) ? file(local.root_env_path) : "") :
    regex("^([^=]+)=(.*)$", trimspace(line))
    if trimspace(line) != "" && !startswith(trimspace(line), "#")
    && can(regex("^([^=]+)=(.*)$", trimspace(line)))
  ] : trimspace(pair[0]) => trimspace(pair[1]) }

  secrets = local.root_secrets

  # Bucket-name secrets: any .env key suffixed _BUCKET_NAME is exposed as a
  # local named. See docs/adr/0006-automatic-bucket-name-locals.md.
  bucket_name_secrets = {
    for k, v in local.secrets : "${lower(k)}" => get_env(k, v)
    if endswith(k, "_BUCKET_NAME")
  }
}

generate "bucket_names" {
  path      = "bucket_names_generated.tf"
  if_exists = "overwrite"
  contents  = <<EOF
locals {
${join("\n", [for k, v in local.bucket_name_secrets : "  ${k} = \"${v}\""])}
}
EOF
}

# Region/project name locals for noisypigeon.com's DigitalOcean leaves,
# defined in the real Terraform file digitalocean/env.tf and injected here
# — a real .tf file's `locals{}` block, not Terragrunt-only locals (which
# are only usable within this .hcl file itself, e.g. in `generate` content
# interpolation, and are NOT automatically exposed as `local.x` to actual
# Terraform code). See docs/adr/0001-terragrunt-terraform-bootstrap.md and
# docs/adr/0009-per-provider-root-hcl.md.
generate "env" {
  path      = "env_generated.tf"
  if_exists = "overwrite"
  contents  = file(find_in_parent_folders("env.tf"))
}

generate "provider" {
  path      = "provider_generated.tf"
  if_exists = "overwrite"
  contents  = <<EOF
terraform {
  required_providers {
    digitalocean = {
      source  = "digitalocean/digitalocean"
      version = "~> 2.0"
    }
  }
}

provider "digitalocean" {
  token             = "${get_env("DIGITALOCEAN_TOKEN", lookup(local.secrets, "DIGITALOCEAN_TOKEN", ""))}"
  spaces_access_id  = "${get_env("DIGITALOCEAN_SPACES_ACCESS_ID", lookup(local.secrets, "DIGITALOCEAN_SPACES_ACCESS_ID", ""))}"
  spaces_secret_key = "${get_env("DIGITALOCEAN_SPACES_SECRET_KEY", lookup(local.secrets, "DIGITALOCEAN_SPACES_SECRET_KEY", ""))}"
}
EOF
}

# Configure backend to use the Spaces bucket
remote_state {
  backend = "s3"

  config = {
    endpoint = "https://tor1.digitaloceanspaces.com"
    region   = "tor1"
    bucket   = lookup(local.secrets, "DIGITALOCEAN_TERRAFORM_STATE_BUCKET_NAME", "")
    # Prefixed with "digitalocean" so leaves' state keys are unchanged now
    # that this file lives inside digitalocean/ instead of the repo root
    # (path_relative_to_include() would otherwise drop that path segment
    # and require a state migration) — see docs/adr/0009-per-provider-root-hcl.md.
    key                         = "digitalocean/${path_relative_to_include()}/terraform.tfstate"
    skip_credentials_validation = true
    skip_metadata_api_check     = true
    skip_region_validation      = true
    skip_requesting_account_id  = true

    access_key = get_env("DIGITALOCEAN_SPACES_ACCESS_ID", lookup(local.secrets, "DIGITALOCEAN_SPACES_ACCESS_ID", ""))
    secret_key = get_env("DIGITALOCEAN_SPACES_SECRET_KEY", lookup(local.secrets, "DIGITALOCEAN_SPACES_SECRET_KEY", ""))
  }

  generate = {
    path      = "backend.tf"
    if_exists = "overwrite"
  }
}
