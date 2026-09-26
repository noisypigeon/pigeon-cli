module "project" {
  source = "git::https://github.com/noisypigeon/pigeon.git//terraform/modules/scaleway/project?ref=terraform/modules/scaleway/project/v0.1.0"
  name   = "noisypigeon.com"
}
