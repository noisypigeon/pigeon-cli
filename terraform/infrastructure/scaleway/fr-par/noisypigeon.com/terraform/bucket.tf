module "bucket" {
  source            = "git::https://github.com/noisypigeon/pigeon.git//terraform/modules/scaleway/object-bucket?ref=terraform/modules/scaleway/object-bucket/v0.1.0"
  enable_versioning = true
  namespace         = "terraform"
  name              = "state"
}
