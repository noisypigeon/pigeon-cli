resource "digitalocean_project" "project" {
  name        = var.name
  environment = var.environment
  purpose     = var.purpose
  is_default  = var.is_default
  description = var.description
}
