variable "name" {
  type        = string
  description = "Project name"
  default     = null
}

variable "description" {
  type        = string
  description = "Project description"
  default     = null
}

variable "organization_id" {
  type        = string
  description = "Organization ID for the project — defaults to the provider's own; changing this recreates the resource"
  default     = null
}
