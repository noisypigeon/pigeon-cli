module "bucket" {
  source            = "git::https://github.com/noisypigeon/pigeon.git//terraform/modules/scaleway/object-bucket?ref=terraform/modules/scaleway/object-bucket/v0.1.0"
  namespace         = "data"
  name              = "email-archive"
  storage_class     = "glacier"
  enable_versioning = true
}
