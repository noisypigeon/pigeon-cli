# DNS records to point noisypigeon.com (apex + www) to noisypigeon.github.io

resource "cloudflare_dns_record" "root_cname" {
  zone_id = local.cloudflare_noisypigeon_com_zone_id
  name    = "@"
  type    = "CNAME"
  content = "noisypigeon.github.io"
  proxied = false
  ttl     = 1
  comment = "github pages root cname (apex, cloudflare-flattened)"
}

resource "cloudflare_dns_record" "www_cname" {
  zone_id = local.cloudflare_noisypigeon_com_zone_id
  name    = "www"
  type    = "CNAME"
  content = "noisypigeon.github.io"
  proxied = false
  ttl     = 1
  comment = "github pages www cname"
}
