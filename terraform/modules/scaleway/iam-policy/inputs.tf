variable "name" {
  type        = string
  description = "IAM application/policy name"

  validation {
    condition = var.name != "" && (
      (var.organization_id != null && length(coalesce(var.organization_permission_sets, [])) > 0) ||
      (var.project_ids != null && length(coalesce(var.project_permission_sets, [])) > 0) ||
      (length(var.bucket_names) > 0 && length(var.bucket_actions) > 0)
    )
    error_message = "name must be non-empty, and at least one of organization_id+organization_permission_sets, project_ids+project_permission_sets, or bucket_names+bucket_actions must be fully set."
  }
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

variable "organization_permission_sets" {
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

variable "bucket_names" {
  type        = map(string)
  description = "Map of static logical key => exact Object Storage bucket name to grant access to (no bucket access is granted by default)"
  default     = {}
}

variable "bucket_actions" {
  type        = list(string)
  description = "S3 actions granted on each bucket in bucket_names (no actions are granted by default)"
  default     = []

  validation {
    condition = alltrue([
      for action in var.bucket_actions : contains([
        "s3:ListBucket",
        "s3:GetObject",
        "s3:PutObject",
        "s3:DeleteObject",
      ], action)
    ])
    error_message = "bucket_actions may only contain: s3:ListBucket, s3:GetObject, s3:PutObject, s3:DeleteObject."
  }
}
