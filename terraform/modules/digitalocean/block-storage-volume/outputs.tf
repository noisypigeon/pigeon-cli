# outputs.tf
output "ids" {
  value       = digitalocean_volume.volume[*].id
  description = "IDs of the created volumes"
}

output "urns" {
  value       = digitalocean_volume.volume[*].urn
  description = "URNs of the created volumes"
}

output "names" {
  value       = digitalocean_volume.volume[*].name
  description = "Names of the created volumes"
}

output "device_ids" {
  value       = [for v in digitalocean_volume.volume : "/dev/disk/by-id/scsi-0DO_Volume_${v.name}"]
  description = "Stable by-id device paths, for use in cloud-init or downstream LVM setup"
}
