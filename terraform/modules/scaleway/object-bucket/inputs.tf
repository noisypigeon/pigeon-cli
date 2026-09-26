#
# Buckets follow a consistent naming scheme.
# Format: {namespace}-{random_code}-{name}
# I.e. example-com-q82q17-sample
#
variable "namespace" {
  type        = string
  description = "Bucket name prefix"
}

variable "name" {
  type        = string
  description = "Bucket name suffix"
}

variable "enable_versioning" {
  type        = bool
  description = "Object versioning enabled (true/false)"
  default     = false
}

variable "storage_class" {
  type        = string
  description = "Storage class for new objects (standard/glacier)"
  default     = "standard"

  validation {
    condition     = contains(["standard", "glacier"], var.storage_class)
    error_message = "storage_class must be \"standard\" or \"glacier\"."
  }
}
