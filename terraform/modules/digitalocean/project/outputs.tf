output "id" {
  description = "ID of Digital Ocean project"
  value       = digitalocean_project.project.id
}

output "name" {
  description = "Name of Digital Ocean project"
  value       = digitalocean_project.project.name
}
