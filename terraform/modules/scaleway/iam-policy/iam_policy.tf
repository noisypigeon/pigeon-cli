resource "scaleway_iam_application" "application" {
  name        = "${var.name}-application"
  description = var.description
}

resource "scaleway_iam_policy" "policy" {
  count = (
    (var.organization_id != null && length(coalesce(var.organization_permission_sets, [])) > 0) ||
    (var.project_ids != null && length(coalesce(var.project_permission_sets, [])) > 0)
  ) ? 1 : 0

  name           = "${var.name}-policy"
  description    = var.description
  application_id = scaleway_iam_application.application.id

  dynamic "rule" {
    for_each = (var.organization_id != null && length(coalesce(var.organization_permission_sets, [])) > 0) ? [1] : []
    content {
      organization_id       = var.organization_id
      permission_set_names  = var.organization_permission_sets
    }
  }

  dynamic "rule" {
    for_each = (var.project_ids != null && length(coalesce(var.project_permission_sets, [])) > 0) ? [1] : []
    content {
      project_ids           = var.project_ids
      permission_set_names  = var.project_permission_sets
    }
  }
}

moved {
  from = scaleway_iam_policy.policy
  to   = scaleway_iam_policy.policy[0]
}

resource "scaleway_iam_api_key" "api_key" {
  application_id = scaleway_iam_application.application.id
  description    = var.description
  expires_at     = var.expires_at
}
