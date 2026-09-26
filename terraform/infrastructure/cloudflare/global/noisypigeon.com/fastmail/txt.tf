resource "cloudflare_dns_record" "spf" {
  zone_id = local.cloudflare_noisypigeon_com_zone_id
  name    = "@"
  type    = "TXT"
  content = "\"v=spf1 include:spf.messagingengine.com ?all\""
  ttl     = 1
  comment = "fastmail spf"
}
