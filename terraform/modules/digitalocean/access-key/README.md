# access-key

A DigitalOcean Spaces access key (`digitalocean_spaces_key`), optionally scoped to one or more buckets.

<!-- BEGIN_TF_DOCS -->
## Inputs

| Name | Description | Type | Default | Required |
|------|-------------|------|---------|:--------:|
| <a name="input_bucket_grants"></a> [bucket\_grants](#input\_bucket\_grants) | Bucket names to grant access to when scoped; defaults to a single grant for `name` | `list(string)` | `null` | no |
| <a name="input_is_bucket_scoped"></a> [is\_bucket\_scoped](#input\_is\_bucket\_scoped) | Restrict key access to specific buckets (true) or grant full account access (false) | `bool` | `true` | no |
| <a name="input_name"></a> [name](#input\_name) | Base name for the access key and default bucket grant | `string` | n/a | yes |
| <a name="input_permission"></a> [permission](#input\_permission) | Spaces permission granted to each bucket (e.g. read, readwrite) | `string` | `"readwrite"` | no |

## Outputs

| Name | Description |
|------|-------------|
| <a name="output_access_key"></a> [access\_key](#output\_access\_key) | Spaces access key ID |
| <a name="output_secret_key"></a> [secret\_key](#output\_secret\_key) | Spaces access key secret |
<!-- END_TF_DOCS -->
