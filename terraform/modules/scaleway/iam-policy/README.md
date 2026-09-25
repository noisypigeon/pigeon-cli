# iam-policy

A Scaleway `scaleway_iam_application` and `scaleway_iam_policy` wrapper to produce a restricted `scaleway_iam_api_key` using permission sets.

<!-- BEGIN_TF_DOCS -->
## Inputs

| Name | Description | Type | Default | Required |
|------|-------------|------|---------|:--------:|
| <a name="input_description"></a> [description](#input\_description) | IAM application/policy description | `string` | `null` | no |
| <a name="input_expires_at"></a> [expires\_at](#input\_expires\_at) | API key expiration timestamp (i.e. 2027-09-25T22:32:12Z) | `string` | `null` | no |
| <a name="input_name"></a> [name](#input\_name) | IAM application/policy name | `string` | n/a | yes |
| <a name="input_org_permission_sets"></a> [org\_permission\_sets](#input\_org\_permission\_sets) | Organization permission set grants | `list(string)` | `null` | no |
| <a name="input_organization_id"></a> [organization\_id](#input\_organization\_id) | Organization id | `string` | `null` | no |
| <a name="input_project_ids"></a> [project\_ids](#input\_project\_ids) | Project ids | `list(string)` | `null` | no |
| <a name="input_project_permission_sets"></a> [project\_permission\_sets](#input\_project\_permission\_sets) | Project permission set grants | `list(string)` | `null` | no |

## Outputs

| Name | Description |
|------|-------------|
| <a name="output_access_key"></a> [access\_key](#output\_access\_key) | IAM API key access key |
| <a name="output_secret_key"></a> [secret\_key](#output\_secret\_key) | IAM API key secret key |
<!-- END_TF_DOCS -->
