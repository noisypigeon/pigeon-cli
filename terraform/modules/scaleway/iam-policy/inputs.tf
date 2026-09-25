variable "name" {
  type        = string
  description = "IAM application/policy name"
}

variable "description" {
  type        = string
  description = "IAM application/policy description"
  default     = null
}

variable "project_ids" {
  type        = list(string)
  description = "Project ids"
  default     = null
}

variable "permission_set_names" {
  type        = list(string)
  description = "Grant access to permission set names"
}
