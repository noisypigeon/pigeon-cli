# project

A thin wrapper around `digitalocean_project`.

<!-- BEGIN_TF_DOCS -->
## Inputs

| Name | Description | Type | Default | Required |
|------|-------------|------|---------|:--------:|
| <a name="input_description"></a> [description](#input\_description) | Project description | `string` | `null` | no |
| <a name="input_environment"></a> [environment](#input\_environment) | Deployment environment (Production, Staging, or Development) | `string` | n/a | yes |
| <a name="input_is_default"></a> [is\_default](#input\_is\_default) | Default-project flag (true/false) | `bool` | `false` | no |
| <a name="input_name"></a> [name](#input\_name) | Project name | `string` | n/a | yes |
| <a name="input_purpose"></a> [purpose](#input\_purpose) | Project purpose | `string` | n/a | yes |

## Outputs

| Name | Description |
|------|-------------|
| <a name="output_id"></a> [id](#output\_id) | Project ID |
| <a name="output_name"></a> [name](#output\_name) | Project name |
<!-- END_TF_DOCS -->
