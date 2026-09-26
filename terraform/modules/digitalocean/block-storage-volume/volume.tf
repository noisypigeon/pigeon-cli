# volume.tf
resource "digitalocean_volume" "volume" {
  count                   = var.volume_count
  region                  = var.region
  name                    = var.volume_count == 1 ? "${var.droplet_name}-volume" : "${var.droplet_name}-volume-${count.index}"
  size                    = var.size
  initial_filesystem_type = var.initial_filesystem_type
}

resource "digitalocean_volume_attachment" "volume_attachment" {
  count      = var.volume_count
  droplet_id = var.droplet_id
  volume_id  = digitalocean_volume.volume[count.index].id
}
