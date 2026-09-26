module "project" {
  source      = "git::https://github.com/noisypigeon/pigeon.git//terraform/modules/digitalocean/project?ref=terraform/modules/digitalocean/project/v0.1.0"
  name        = local.data_project
  environment = "Production"
  purpose     = "Operational / Object storage"
}
