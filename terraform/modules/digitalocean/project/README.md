# project

A thin wrapper around `digitalocean_project`.

<!-- BEGIN_TF_DOCS -->
## Inputs

| Name | Description | Type | Default | Required |
|------|-------------|------|---------|:--------:|
| <a name="input_description"></a> [description](#input\_description) | Description of Digital Ocean project | `string` | `null` | no |
| <a name="input_environment"></a> [environment](#input\_environment) | Values: Production, Staging, Development | `string` | n/a | yes |
| <a name="input_is_default"></a> [is\_default](#input\_is\_default) | Is default project (true/false) | `bool` | `false` | no |
| <a name="input_name"></a> [name](#input\_name) | Name of Digital Ocean project | `string` | n/a | yes |
| <a name="input_purpose"></a> [purpose](#input\_purpose) | Purpose of Digital Ocean project | `string` | n/a | yes |

## Outputs

| Name | Description |
|------|-------------|
| <a name="output_id"></a> [id](#output\_id) | ID of Digital Ocean project |
| <a name="output_name"></a> [name](#output\_name) | Name of Digital Ocean project |
<!-- END_TF_DOCS -->
