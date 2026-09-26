module "bucket" {
  source  = "git::https://github.com/noisypigeon/pigeon.git//terraform/modules/digitalocean/cold-storage-bucket?ref=terraform/modules/digitalocean/cold-storage-bucket/v0.1.0"
  name    = local.data_custodian_m5q4wp_bucket_name
  region  = local.tor1_region
  project = local.data_project
}
