# object-bucket

A Scaleway Object Storage bucket (`scaleway_object_bucket`), with versioning and a standard/glacier storage-class toggle implemented via an immediate lifecycle transition.

<!-- BEGIN_TF_DOCS -->
## Inputs

| Name | Description | Type | Default | Required |
|------|-------------|------|---------|:--------:|
| <a name="input_enable_versioning"></a> [enable\_versioning](#input\_enable\_versioning) | Object versioning enabled (true/false) | `bool` | `false` | no |
| <a name="input_name"></a> [name](#input\_name) | Bucket name | `string` | n/a | yes |
| <a name="input_storage_class"></a> [storage\_class](#input\_storage\_class) | Storage class for new objects (standard/glacier) | `string` | `"standard"` | no |

## Outputs

| Name | Description |
|------|-------------|
| <a name="output_endpoint"></a> [endpoint](#output\_endpoint) | Bucket endpoint URL |
| <a name="output_id"></a> [id](#output\_id) | Bucket ID |
| <a name="output_name"></a> [name](#output\_name) | Bucket name |
<!-- END_TF_DOCS -->
