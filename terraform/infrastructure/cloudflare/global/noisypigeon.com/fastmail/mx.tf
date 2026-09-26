resource "cloudflare_dns_record" "mx_primary" {
  zone_id  = local.cloudflare_noisypigeon_com_zone_id
  name     = "@"
  type     = "MX"
  content  = "in1-smtp.messagingengine.com"
  priority = 10
  ttl      = 1 # Auto
  comment  = "fastmail primary mx"
}

resource "cloudflare_dns_record" "mx_secondary" {
  zone_id  = local.cloudflare_noisypigeon_com_zone_id
  name     = "@"
  type     = "MX"
  content  = "in2-smtp.messagingengine.com"
  priority = 20
  ttl      = 1 # Auto
  comment  = "fastmail secondary mx"
}
