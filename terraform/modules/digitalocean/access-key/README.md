# access-key

A DigitalOcean Spaces access key (`digitalocean_spaces_key`), optionally scoped to one or more buckets.

<!-- BEGIN_TF_DOCS -->
## Inputs

| Name | Description | Type | Default | Required |
| ---- | ----------- | ---- | ------- | :------: |
| <a name="input_bucket_grants"></a> [bucket\_grants](#input\_bucket\_grants) | List of bucket names to grant access to. If null, uses name variable for backward compatibility. | `list(string)` | `null` | no |
| <a name="input_is_bucket_scoped"></a> [is\_bucket\_scoped](#input\_is\_bucket\_scoped) | limit key access to the bucket | `bool` | `true` | no |
| <a name="input_name"></a> [name](#input\_name) | The name of the bucket | `string` | n/a | yes |
| <a name="input_permission"></a> [permission](#input\_permission) | permission set | `string` | `"readwrite"` | no |

## Outputs

| Name | Description |
| ---- | ----------- |
| <a name="output_access_key"></a> [access\_key](#output\_access\_key) | The access key ID of the key |
| <a name="output_secret_key"></a> [secret\_key](#output\_secret\_key) | The access key secret of the key |
<!-- END_TF_DOCS -->
