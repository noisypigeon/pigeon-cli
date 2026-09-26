data "digitalocean_ssh_key" "ssh_key" {
  name = var.ssh_key_name
}
