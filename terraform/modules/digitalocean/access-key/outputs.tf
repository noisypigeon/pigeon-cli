output "access_key" {
  description = "The access key ID of the key"
  value       = digitalocean_spaces_key.bucket_key.access_key
  sensitive   = true
}

output "secret_key" {
  description = "The access key secret of the key"
  value       = digitalocean_spaces_key.bucket_key.secret_key
  sensitive   = true
}
