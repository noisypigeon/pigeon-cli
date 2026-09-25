resource "scaleway_iam_application" "application" {
  name        = "${var.name}-application"
  description = var.description
}

resource "scaleway_iam_policy" "policy" {
  name           = "${var.name}-policy"
  description    = var.description
  application_id = scaleway_iam_application.application.id

  rule {
    project_ids           = var.project_ids
    permission_set_names  = var.permission_set_names
  }
}

resource "scaleway_iam_api_key" "api_key" {
  application_id = scaleway_iam_application.application.id
  description    = var.description
}
