variable "name" {
  type        = string
  description = "Bucket name suffix"
}

variable "region" {
  type        = string
  description = "Bucket region"
}

variable "project" {
  type        = string
  description = "Project this bucket is attached to"
  default     = null
}
