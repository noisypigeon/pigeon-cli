resource "scaleway_account_project" "project" {
  name            = var.name
  description     = var.description
  organization_id = var.organization_id
}
