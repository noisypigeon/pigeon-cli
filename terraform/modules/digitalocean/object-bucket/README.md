# object-bucket

A DigitalOcean Spaces bucket (`digitalocean_spaces_bucket`) with a randomized name suffix.

<!-- BEGIN_TF_DOCS -->
## Inputs

| Name | Description | Type | Default | Required |
| ---- | ----------- | ---- | ------- | :------: |
| <a name="input_acl"></a> [acl](#input\_acl) | Bucket ACL policy | `string` | `"private"` | no |
| <a name="input_enable_versioning"></a> [enable\_versioning](#input\_enable\_versioning) | Object versioning enabled (true/false) | `bool` | `false` | no |
| <a name="input_name"></a> [name](#input\_name) | Bucket name suffix | `string` | n/a | yes |
| <a name="input_namespace"></a> [namespace](#input\_namespace) | Bucket name prefix | `string` | n/a | yes |
| <a name="input_project"></a> [project](#input\_project) | Name of Digital Ocean project bucket is associated to | `string` | n/a | yes |
| <a name="input_region"></a> [region](#input\_region) | Bucket region | `string` | n/a | yes |

## Outputs

| Name | Description |
| ---- | ----------- |
| <a name="output_name"></a> [name](#output\_name) | Bucket computed name |
| <a name="output_urn"></a> [urn](#output\_urn) | Bucket URN |
<!-- END_TF_DOCS -->
