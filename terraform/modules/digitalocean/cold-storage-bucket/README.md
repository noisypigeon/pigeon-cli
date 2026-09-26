# cold-storage-bucket

A data-source wrapper for a DigitalOcean Spaces Cold Storage bucket (not yet supported as a Terraform resource by the DO provider — the bucket is created click-ops and managed as a data source), optionally attached to a project.

<!-- BEGIN_TF_DOCS -->
## Inputs

| Name | Description | Type | Default | Required |
|------|-------------|------|---------|:--------:|
| <a name="input_name"></a> [name](#input\_name) | Bucket name suffix | `string` | n/a | yes |
| <a name="input_project"></a> [project](#input\_project) | Project this bucket is attached to | `string` | `null` | no |
| <a name="input_region"></a> [region](#input\_region) | Bucket region | `string` | n/a | yes |

## Outputs

| Name | Description |
|------|-------------|
| <a name="output_name"></a> [name](#output\_name) | Computed bucket name |
<!-- END_TF_DOCS -->
