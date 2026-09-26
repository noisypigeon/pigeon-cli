locals {
  # Shared locals inherited by every DigitalOcean leaf under this provider
  # via digitalocean/root.hcl's `generate "env"` block. See
  # docs/adr/0001-terragrunt-terraform-bootstrap.md and
  # docs/adr/0009-per-provider-root-hcl.md.

  # Digital Ocean Regions
  tor1_region = "tor1"

  # Digital Ocean Projects
  management_project  = "Management"
  data_project        = "Data"
}
