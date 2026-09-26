# module "iam" {
#   source = "git::https://github.com/noisypigeon/pigeon.git//terraform/modules/scaleway/iam-policy?ref=terraform/modules/scaleway/iam-policy/v0.1.0"
#   name   = "${module.bucket.name}-iam"
#   project_ids = [
#     local.scaleway_project_id_noisypigeon_com
#   ]
#   project_permission_sets = [
#     "ObjectStorageObjectsWrite",
#     "ObjectStorageObjectsRead"
#   ]
#   expires_at = "2027-09-25T22:32:12Z"
#   bucket_names = {
#     email = module.bucket.name
#   }
#   bucket_actions = [
#     "s3:ListBucket",
#     "s3:GetObject",
#     "s3:PutObject"
#   ]
# }

# output "access_key" {
#   description = "IAM API key access key"
#   value       = module.iam.access_key
#   sensitive   = true
# }

# output "secret_key" {
#   description = "IAM API key secret key"
#   value       = module.iam.secret_key
#   sensitive   = true
# }
