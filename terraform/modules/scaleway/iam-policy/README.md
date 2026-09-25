# iam-policy

A Scaleway `scaleway_iam_application` and `scaleway_iam_policy` wrapper to produce a restricted `scaleway_iam_api_key` using permission sets.

<!-- BEGIN_TF_DOCS -->
## Inputs

| Name | Description | Type | Default | Required |
|------|-------------|------|---------|:--------:|
| <a name="input_description"></a> [description](#input\_description) | IAM application/policy description | `string` | `null` | no |
| <a name="input_name"></a> [name](#input\_name) | IAM application/policy name | `string` | n/a | yes |
| <a name="input_permission_set_names"></a> [permission\_set\_names](#input\_permission\_set\_names) | Grant access to permission set names | `list(string)` | n/a | yes |
| <a name="input_project_ids"></a> [project\_ids](#input\_project\_ids) | Project ids | `list(string)` | `null` | no |

## Outputs

| Name | Description |
|------|-------------|
| <a name="output_scw_access_key"></a> [scw\_access\_key](#output\_scw\_access\_key) | IAM API key access key |
| <a name="output_scw_secret_key"></a> [scw\_secret\_key](#output\_scw\_secret\_key) | IAM API key secret key |
<!-- END_TF_DOCS -->
