#
# Droplets follow the same naming scheme as buckets.
# Format: {namespace}-{random_code}-{name}
# I.e. example-com-q82q17-sample
#
variable "image" {
  type        = string
  description = "Droplet image (slug or ID)"
}

variable "name" {
  type        = string
  description = "Droplet name suffix"
}

variable "user_name" {
  type        = string
  description = "Sudo-enabled user created on the droplet"
  default     = "pigeon"
}

variable "buckets" {
  type = list(object({
    bucket_name  = string
    bucket_alias = string
  }))
  description = "Buckets to configure in rclone (name + alias)"

  validation {
    condition     = length(var.buckets) > 0
    error_message = "At least one bucket must be specified."
  }

  validation {
    condition     = alltrue([for b in var.buckets : can(regex("^[a-z0-9][a-z0-9-]*[a-z0-9]$", b.bucket_alias))])
    error_message = "Bucket aliases must be lowercase alphanumeric with hyphens."
  }

  validation {
    condition     = length(var.buckets) == length(distinct([for b in var.buckets : b.bucket_alias]))
    error_message = "Bucket aliases must be unique."
  }
}

variable "namespace" {
  type        = string
  description = "Droplet name prefix"
}

variable "region" {
  type        = string
  description = "Droplet region"
  default     = "tor1"
}

variable "size" {
  type        = string
  description = "Droplet size slug"
}

variable "cloudflare_zone_id" {
  type        = string
  description = "Cloudflare zone ID"
}

variable "public_networking" {
  type        = bool
  description = "Public networking enabled (true/false)"
  default     = true
}

variable "ssh_key_name" {
  type        = string
  description = "SSH key name to grant droplet access"
}

variable "lvm_mount_point" {
  type        = string
  description = "LVM mount point for combined attached volumes"
  default     = "/mnt/data"
}

variable "lvm_filesystem" {
  type        = string
  description = "LVM filesystem type"
  default     = "xfs"
}
