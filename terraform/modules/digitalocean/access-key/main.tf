resource "digitalocean_spaces_key" "bucket_key" {
  name = "${var.name}-key"

  dynamic "grant" {
    for_each = var.is_bucket_scoped ? (var.bucket_grants != null ? var.bucket_grants : [var.name]) : [""]
    content {
      bucket     = grant.value
      permission = var.permission
    }
  }
}
