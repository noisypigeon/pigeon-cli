resource "scaleway_object_bucket" "bucket" {
  name = var.name

  versioning {
    enabled = var.enable_versioning
  }

  dynamic "lifecycle_rule" {
    for_each = var.storage_class == "glacier" ? [1] : []
    content {
      enabled = true

      transition {
        days          = 0
        storage_class = "GLACIER"
      }
    }
  }
}
