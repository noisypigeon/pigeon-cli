module "project" {
  source      = "git::https://github.com/noisypigeon/pigeon.git//terraform/modules/digitalocean/project?ref=terraform/modules/digitalocean/project/v0.1.0"
  name        = local.management_project
  environment = "Production"
  purpose     = "Operational / Developer tooling"
  is_default  = true
}
