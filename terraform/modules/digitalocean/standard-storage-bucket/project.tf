data "digitalocean_project" "project" {
  name = var.project
}

resource "digitalocean_project_resources" "this" {
  project   = data.digitalocean_project.project.id
  resources = [digitalocean_spaces_bucket.bucket.urn]
}
