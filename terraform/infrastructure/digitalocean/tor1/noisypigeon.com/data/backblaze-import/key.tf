module "key" {
  source           = "git::https://github.com/noisypigeon/pigeon.git//terraform/modules/digitalocean/access-key?ref=terraform/modules/digitalocean/access-key/v0.1.0"
  name             = module.bucket.name
  permission       = "read"
}

# output "access_key" {
#   description = "Spaces access key ID"
#   value       = module.key.access_key
#   sensitive   = true
# }

# output "secret_key" {
#   description = "Spaces access key secret"
#   value       = module.key.secret_key
#   sensitive   = true
# }
