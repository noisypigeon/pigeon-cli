output "id" {
  description = "Bucket ID"
  value       = scaleway_object_bucket.bucket.id
}

output "name" {
  description = "Bucket name"
  value       = scaleway_object_bucket.bucket.name
}

output "endpoint" {
  description = "Bucket endpoint URL"
  value       = scaleway_object_bucket.bucket.endpoint
}
