resource "cloudflare_zone" "noisypigeon_com" {
  account = {
    id = local.cloudflare_account_id
  }
  name = "noisypigeon.com"
  type = "full"
}
