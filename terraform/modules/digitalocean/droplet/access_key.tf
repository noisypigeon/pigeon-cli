module "compute_bucket_access_key" {
  source = "git::https://github.com/noisypigeon/pigeon.git//terraform/modules/digitalocean/access-key?ref=terraform/modules/digitalocean/access-key/v0.1.0"
  name   = "${var.namespace}-${random_string.suffix.result}-${var.name}"

  bucket_grants = local.bucket_names
}
