module "bucket" {
  source    = "git::https://github.com/noisypigeon/pigeon.git//terraform/modules/digitalocean/standard-storage-bucket?ref=terraform/modules/digitalocean/standard-storage-bucket/v0.1.0"
  namespace = "terraform"
  name      = "state"
  project   = local.management_project
  region    = local.tor1_region
}
