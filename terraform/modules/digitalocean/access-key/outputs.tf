output "access_key" {
  description = "Spaces access key ID"
  value       = digitalocean_spaces_key.bucket_key.access_key
  sensitive   = true
}

output "secret_key" {
  description = "Spaces access key secret"
  value       = digitalocean_spaces_key.bucket_key.secret_key
  sensitive   = true
}
