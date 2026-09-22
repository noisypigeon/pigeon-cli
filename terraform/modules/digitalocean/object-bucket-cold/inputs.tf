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
  description = "Name of Digital Ocean project bucket is associated to"
  default     = null
}
