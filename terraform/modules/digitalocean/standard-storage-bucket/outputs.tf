output "urn" {
  description = "Bucket URN"
  value       = digitalocean_spaces_bucket.bucket.urn
}

output "name" {
  description = "Computed bucket name"
  value       = digitalocean_spaces_bucket.bucket.name
}
