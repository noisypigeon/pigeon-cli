variable "name" {
  type        = string
  description = "The name of the bucket"
}

variable "permission" {
  type        = string
  description = "permission set"
  default     = "readwrite"
}

variable "is_bucket_scoped" {
  type        = bool
  description = "limit key access to the bucket"
  default     = true
}

variable "bucket_grants" {
  type        = list(string)
  description = "List of bucket names to grant access to. If null, uses name variable for backward compatibility."
  default     = null
}
