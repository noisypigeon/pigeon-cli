# block-storage-volume

One or more DigitalOcean Block Storage volumes (`digitalocean_volume`), attached to a droplet (`digitalocean_volume_attachment`).

<!-- BEGIN_TF_DOCS -->
## Inputs

| Name | Description | Type | Default | Required |
|------|-------------|------|---------|:--------:|
| <a name="input_droplet_id"></a> [droplet\_id](#input\_droplet\_id) | ID of the droplet the volume(s) attach to | `string` | n/a | yes |
| <a name="input_droplet_name"></a> [droplet\_name](#input\_droplet\_name) | Name of the droplet the volume(s) attach to | `string` | n/a | yes |
| <a name="input_initial_filesystem_type"></a> [initial\_filesystem\_type](#input\_initial\_filesystem\_type) | Filesystem to pre-format each volume with — leave null when combining volumes via LVM/mdadm, since pre-formatting breaks that | `string` | `null` | no |
| <a name="input_region"></a> [region](#input\_region) | DigitalOcean region for the volume(s) | `string` | `"tor1"` | no |
| <a name="input_size"></a> [size](#input\_size) | Size of each volume, in GiB | `number` | n/a | yes |
| <a name="input_volume_count"></a> [volume\_count](#input\_volume\_count) | Number of volumes to create — use >1 to combine via LVM/mdadm on the droplet | `number` | `1` | no |

## Outputs

| Name | Description |
|------|-------------|
| <a name="output_device_ids"></a> [device\_ids](#output\_device\_ids) | Stable by-id device paths, for use in cloud-init or downstream LVM setup |
| <a name="output_ids"></a> [ids](#output\_ids) | IDs of the created volumes |
| <a name="output_names"></a> [names](#output\_names) | Names of the created volumes |
| <a name="output_urns"></a> [urns](#output\_urns) | URNs of the created volumes |
<!-- END_TF_DOCS -->
