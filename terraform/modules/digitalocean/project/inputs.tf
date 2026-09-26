variable "name" {
  type        = string
  description = "Project name"
}

variable "environment" {
  type        = string
  description = "Deployment environment (Production, Staging, or Development)"
}

variable "purpose" {
  type        = string
  description = "Project purpose"
}

variable "description" {
  type        = string
  description = "Project description"
  default     = null
}

variable "is_default" {
  type        = bool
  description = "Default-project flag (true/false)"
  default     = false
}
