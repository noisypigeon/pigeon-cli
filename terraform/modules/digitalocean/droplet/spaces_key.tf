module "compute_bucket_access" {
  source = "../access-key"
  name   = "${var.namespace}-${random_string.suffix.result}-${var.name}"

  bucket_grants = local.bucket_names
}
