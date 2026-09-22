variable "name" {
  type        = string
  description = "Name of Digital Ocean project"
}

variable "environment" {
  type        = string
  description = "Values: Production, Staging, Development"
}

variable "purpose" {
  type        = string
  description = "Purpose of Digital Ocean project"
}

variable "description" {
  type        = string
  description = "Description of Digital Ocean project"
  default     = null
}

variable "is_default" {
  type        = bool
  description = "Is default project (true/false)"
  default     = false
}
