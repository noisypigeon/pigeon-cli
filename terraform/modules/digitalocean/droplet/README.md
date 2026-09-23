# droplet

A DigitalOcean droplet (`digitalocean_droplet`) with cloud-init provisioning (rclone, an LVM auto-combine script for attached volumes, a sudo user) and a Cloudflare DNS alias.

<!-- BEGIN_TF_DOCS -->
## Inputs

| Name | Description | Type | Default | Required |
| ---- | ----------- | ---- | ------- | :------: |
| <a name="input_buckets"></a> [buckets](#input\_buckets) | Buckets to configure in rclone (name + alias) | <pre>list(object({<br/>    bucket_name  = string<br/>    bucket_alias = string<br/>  }))</pre> | n/a | yes |
| <a name="input_cloudflare_zone_id"></a> [cloudflare\_zone\_id](#input\_cloudflare\_zone\_id) | Cloudflare zone ID | `string` | n/a | yes |
| <a name="input_image"></a> [image](#input\_image) | Droplet image (slug or ID) | `string` | n/a | yes |
| <a name="input_lvm_filesystem"></a> [lvm\_filesystem](#input\_lvm\_filesystem) | LVM filesystem type | `string` | `"xfs"` | no |
| <a name="input_lvm_mount_point"></a> [lvm\_mount\_point](#input\_lvm\_mount\_point) | LVM mount point for combined attached volumes | `string` | `"/mnt/data"` | no |
| <a name="input_name"></a> [name](#input\_name) | Droplet name suffix | `string` | n/a | yes |
| <a name="input_namespace"></a> [namespace](#input\_namespace) | Droplet name prefix | `string` | n/a | yes |
| <a name="input_public_networking"></a> [public\_networking](#input\_public\_networking) | Public networking enabled (true/false) | `bool` | `true` | no |
| <a name="input_region"></a> [region](#input\_region) | Droplet region | `string` | `"tor1"` | no |
| <a name="input_size"></a> [size](#input\_size) | Droplet size slug | `string` | n/a | yes |
| <a name="input_ssh_key_name"></a> [ssh\_key\_name](#input\_ssh\_key\_name) | SSH key name to grant droplet access | `string` | n/a | yes |
| <a name="input_user_name"></a> [user\_name](#input\_user\_name) | Sudo-enabled user created on the droplet | `string` | `"pigeon"` | no |

## Outputs

| Name | Description |
| ---- | ----------- |
| <a name="output_hostname"></a> [hostname](#output\_hostname) | DNS hostname on the Cloudflare zone |
| <a name="output_id"></a> [id](#output\_id) | Droplet ID |
| <a name="output_ipv4_address"></a> [ipv4\_address](#output\_ipv4\_address) | Droplet IPv4 address |
| <a name="output_name"></a> [name](#output\_name) | Computed droplet name |
| <a name="output_urn"></a> [urn](#output\_urn) | Droplet URN |
<!-- END_TF_DOCS -->
