output "name" {
  description = "Bucket computed name"
  value       = data.digitalocean_spaces_bucket.bucket.name
}
