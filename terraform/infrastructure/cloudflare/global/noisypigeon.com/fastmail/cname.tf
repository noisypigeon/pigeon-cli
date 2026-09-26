resource "cloudflare_dns_record" "dkim_fm1" {
  zone_id = local.cloudflare_noisypigeon_com_zone_id
  name    = "fm1._domainkey"
  type    = "CNAME"
  content = "fm1.noisypigeon.com.dkim.fmhosted.com"
  ttl     = 1
  proxied = false
  comment = "fastmail primary dkim"
}

resource "cloudflare_dns_record" "dkim_fm2" {
  zone_id = local.cloudflare_noisypigeon_com_zone_id
  name    = "fm2._domainkey"
  type    = "CNAME"
  content = "fm2.noisypigeon.com.dkim.fmhosted.com"
  ttl     = 1
  proxied = false
  comment = "fastmail secondary dkim"
}

resource "cloudflare_dns_record" "dkim_fm3" {
  zone_id = local.cloudflare_noisypigeon_com_zone_id
  name    = "fm3._domainkey"
  type    = "CNAME"
  content = "fm3.noisypigeon.com.dkim.fmhosted.com"
  ttl     = 1
  proxied = false
  comment = "fastmail tertiary dkim"
}
