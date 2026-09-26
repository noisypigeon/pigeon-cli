resource "scaleway_object_bucket_policy" "bucket_access" {
  for_each = toset(var.bucket_names)

  bucket = each.value
  policy = jsonencode({
    Version = "2023-04-17"
    Statement = [
      {
        Sid       = "IamPolicyBucketAccess"
        Effect    = "Allow"
        Principal = { SCW = "application_id:${scaleway_iam_application.application.id}" }
        Action    = var.bucket_actions
        Resource  = [each.value, "${each.value}/*"]
      }
    ]
  })
}
