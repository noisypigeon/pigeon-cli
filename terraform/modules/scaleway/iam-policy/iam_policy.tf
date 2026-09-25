resource "scaleway_iam_application" "application" {
  name        = "${var.name}-application"
  description = var.description
}

resource "scaleway_iam_policy" "policy" {
  name           = "${var.name}-policy"
  description    = var.description
  application_id = scaleway_iam_application.application.id

  rule {
    organization_id       = var.organization_id
    permission_set_names  = var.org_permission_sets
  }

  rule {
    project_ids           = var.project_ids
    permission_set_names  = var.project_permission_sets
  }
}

resource "scaleway_iam_api_key" "api_key" {
  application_id = scaleway_iam_application.application.id
  description    = var.description
  expires_at     = var.expires_at
}
