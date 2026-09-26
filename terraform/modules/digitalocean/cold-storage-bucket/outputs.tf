output "name" {
  description = "Computed bucket name"
  value       = data.digitalocean_spaces_bucket.bucket.name
}
