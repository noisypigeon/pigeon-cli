# inputs.tf
variable "droplet_name" {
  type        = string
  description = "Name of the droplet the volume(s) attach to"
}

variable "droplet_id" {
  type        = string
  description = "ID of the droplet the volume(s) attach to"
}

variable "size" {
  type        = number
  description = "Size of each volume, in GiB"
}

variable "region" {
  type        = string
  description = "DigitalOcean region for the volume(s)"
  default     = "tor1"
}

variable "volume_count" {
  type        = number
  description = "Number of volumes to create — use >1 to combine via LVM/mdadm on the droplet"
  default     = 1
}

variable "initial_filesystem_type" {
  type        = string
  description = "Filesystem to pre-format each volume with — leave null when combining volumes via LVM/mdadm, since pre-formatting breaks that"
  default     = null
}
