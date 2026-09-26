variable "name" {
  type        = string
  description = "Base name for the access key and default bucket grant"
}

variable "permission" {
  type        = string
  description = "Spaces permission granted to each bucket (e.g. read, readwrite)"
  default     = "readwrite"
}

variable "is_bucket_scoped" {
  type        = bool
  description = "Restrict key access to specific buckets (true) or grant full account access (false)"
  default     = true
}

variable "bucket_grants" {
  type        = list(string)
  description = "Bucket names to grant access to when scoped; defaults to a single grant for `name`"
  default     = null
}
