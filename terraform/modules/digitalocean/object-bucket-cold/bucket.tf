# The Digital Ocean Terraform provider does not support
# Cold Storage buckets yet. The bucket was created
# click-ops and is managed as a data source.

data "digitalocean_spaces_bucket" "bucket" {
  name   = var.name
  region = var.region
}

data "digitalocean_project" "project" {
  name = var.project
}

resource "digitalocean_project_resources" "this" {
  project   = data.digitalocean_project.project.id
  resources = [data.digitalocean_spaces_bucket.bucket.urn]
}
