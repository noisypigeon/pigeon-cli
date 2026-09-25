variable "name" {
  type        = string
  description = "IAM application/policy name"
}

variable "description" {
  type        = string
  description = "IAM application/policy description"
  default     = null
}

variable "organization_id" {
  type        = string
  description = "Organization id"
  default     = null
}

variable "org_permission_sets" {
  type        = list(string)
  description = "Organization permission set grants"
  default     = null
}

variable "project_ids" {
  type        = list(string)
  description = "Project ids"
  default     = null
}

variable "project_permission_sets" {
  type        = list(string)
  description = "Project permission set grants"
  default     = null
}

variable "expires_at" {
  type        = string
  description = "API key expiration timestamp (i.e. 2027-09-25T22:32:12Z)"
  default     = null
}
