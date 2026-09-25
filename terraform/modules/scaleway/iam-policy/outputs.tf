output "scw_access_key" {
  description = "IAM API key access key"
  value       = scaleway_iam_api_key.api_key.access_key
  sensitive   = true
}

output "scw_secret_key" {
  description = "IAM API key secret key"
  value       = scaleway_iam_api_key.api_key.secret_key
  sensitive   = true
}
