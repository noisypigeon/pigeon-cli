data "cloudflare_zone" "zone" {
  filter = {
    id = var.cloudflare_zone_id
  }
}

resource "cloudflare_dns_record" "droplet_alias" {
  zone_id = data.cloudflare_zone.zone.id
  name    = random_string.suffix.result
  type    = "A"
  content = digitalocean_droplet.droplet.ipv4_address
  ttl     = 300           # or 1 for "automatic" if proxied
  proxied = false         # IMPORTANT for SSH — see below
}
