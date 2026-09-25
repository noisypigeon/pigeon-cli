# project

A thin wrapper around `scaleway_account_project`.

<!-- BEGIN_TF_DOCS -->
## Inputs

| Name | Description | Type | Default | Required |
|------|-------------|------|---------|:--------:|
| <a name="input_description"></a> [description](#input\_description) | Project description | `string` | `null` | no |
| <a name="input_name"></a> [name](#input\_name) | Project name | `string` | `null` | no |
| <a name="input_organization_id"></a> [organization\_id](#input\_organization\_id) | Organization ID for the project — defaults to the provider's own; changing this recreates the resource | `string` | `null` | no |

## Outputs

| Name | Description |
|------|-------------|
| <a name="output_id"></a> [id](#output\_id) | Project ID |
| <a name="output_name"></a> [name](#output\_name) | Project name |
<!-- END_TF_DOCS -->
