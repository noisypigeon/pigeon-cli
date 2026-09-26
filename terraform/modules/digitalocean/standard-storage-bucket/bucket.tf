resource "random_string" "suffix" {
  length  = 6
  lower   = true
  upper   = false
  numeric = true
  special = false
}

resource "digitalocean_spaces_bucket" "bucket" {
  name   = "${var.namespace}-${random_string.suffix.result}-${var.name}"
  region = var.region
  acl    = var.acl
  versioning {
    enabled = var.enable_versioning
  }
}
