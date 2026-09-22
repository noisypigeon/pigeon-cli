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

variable "region" {
  type        = string
  description = "Bucket region"
}

variable "acl" {
  type        = string
  description = "Bucket ACL policy"
  default     = "private"
}

variable "enable_versioning" {
  type        = bool
  description = "Object versioning enabled (true/false)"
  default     = false
}

variable "project" {
  type        = string
  description = "Name of Digital Ocean project bucket is associated to"
}
