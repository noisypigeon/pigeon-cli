resource "random_string" "suffix" {
  length  = 6
  lower   = true
  upper   = false
  numeric = true
  special = false
}

resource "scaleway_object_bucket" "bucket" {
  name = "${var.namespace}-${random_string.suffix.result}-${var.name}"

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
