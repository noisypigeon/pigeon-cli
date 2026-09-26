resource "cloudflare_zone" "pigeon_dev" {
  account = {
    id = local.cloudflare_account_id
  }
  name = "pigeon.dev"
  type = "full"
}
