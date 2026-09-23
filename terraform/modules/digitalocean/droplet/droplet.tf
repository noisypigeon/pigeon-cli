resource "digitalocean_droplet" "droplet" {
  image   = var.image
  name    = "${var.namespace}-${random_string.suffix.result}-${var.name}"
  region  = var.region
  size    = var.size
  public_networking = var.public_networking
  ssh_keys = [data.digitalocean_ssh_key.ssh_key.id]
  backups = false

  user_data = <<-EOF
    #cloud-config
    hostname: ${random_string.suffix.result}
    manage_etc_hosts: true
    package_update: true
    package_upgrade: false
    packages:
      - rclone
      - neovim
      - lvm2

    users:
      - name: ${var.user_name}
        groups: sudo
        shell: /bin/bash
        sudo: ['ALL=(ALL) NOPASSWD:ALL']
        ssh_authorized_keys:
          - ${data.digitalocean_ssh_key.ssh_key.public_key}

    write_files:
      - path: /home/${var.user_name}/.config/rclone/rclone.conf
        owner: ${var.user_name}:${var.user_name}
        permissions: '0600'
        defer: true
        content: |
          %{~ for bucket in var.buckets ~}
          [${bucket.bucket_alias}]
          type = alias
          remote = ${bucket.bucket_name}:${bucket.bucket_name}
          [${bucket.bucket_name}]
          type = s3
          provider = DigitalOcean
          access_key_id = ${module.compute_bucket_access.access_key}
          secret_access_key = ${module.compute_bucket_access.secret_key}
          endpoint = ${var.region}.digitaloceanspaces.com
          acl = private
          no_check_bucket = true
          %{~ endfor ~}

      - path: /etc/welcome_message
        encoding: b64
        permissions: '0644'
        overwrite: true
        content: "4qO/4qO/4qO/4qO/4qO/4qO/4qO/4qO/4qO/4qO/4qO/4qO/4qO/4qO/4qO/4qO/4qO/4qO/4qO/4qO/4qO/4qO/4qO/4qO/4qO/4qO/4qO/4qO/4qO/4qO/4qO/4qO/4qO/4qO/4qO/4qO/4qO/4qO/4qO/4qO/4qO/4qO/4qO/4qO94qOr4qKf4qO/4qO/4qO/4qK/CuKju+Kjv+Kjv+Kjv+Kjv+Kjv+Kjv+Kjv+Kjv+Kjv+Kjv+Kjv+Kjv+Kjv+Kjv+Kjv+Kjv+Kjv+Kjv+Kjv+Kjv+Kjv+Kjv+Kjv+Kjv+Kjv+Kjv+Kjv+Kjv+Kjv+Kjv+Kjv+Kjv+Kjv+Kjv+Kjv+Kjv+Kjv+Kjv+Kjv+Kjv+Kjv+Kjv+Kjv+Kjv+Kjv+Kjv+Kjv+Kjv+Kjngriob/io7/io7/io7/io7/io7/io7/io7/io7/io5/io7/io63io6/io73io7/io7/io7/io7/io7/io7/io7/io7/io7/io7/io7/io7/io7/io7/io7/io7/io7/io7/io7/io7/io7/io7/io7/io7/io7/io7/io7/io7/io7/io7/io7/io7/io7/io7/io7/io78K4qO84qO/4qO/4qO/4qO/4qO/4qO/4qO/4qK34qG/4qCL4qCJ4qOb4qO74qO/4qC/4qGa4qK74qO/4qO/4qO/4qO/4qO/4qO/4qO/4qO/4qO/4qO/4qO/4qO/4qO/4qO/4qO/4qO/4qO/4qO/4qO/4qO/4qO/4qO/4qO/4qO/4qO/4qO/4qO/4qO/4qO/4qO/4qO/4qO/CuKjv+Kjv+Kjv+Kjv+Kjv+Kjv+Kju+Khn+KjhOKggOKgpOKhnOKhu+Kih+Kgv+Kih+KiuOKjoOKgmOKgm+Kjv+Kjv+Kjv+Kjv+Kjv+Kjv+Kjv+Kjv+Kjv+Kjv+Kjv+Kjv+Kjv+Kjv+Kjv+Kjv+Kjv+Kjv+Kjv+Kjv+Kjv+Kjv+Kjv+Kjv+Kjv+Kjv+Kjv+Kjv+Kjv+Kjvwrio7/io7/io7/io7/io7/io7/io5/ioorio6TioaTio4TioYDioJHioKDioIjiopzioIbio4HiorjioKLioo7io7fio7/io7fio7bio7bio7/iob/io7/io5/io7/io7/io7/io7/io7/io7/io7/io7/io7/io7/io7/io7/io7/io7/io7/io7/io7/io7/io7/io78K4qO/4qO/4qO/4qO/4qO/4qO/4qO/4qO/4qO/4qO/4qO94qO34qGA4qCA4qCA4qCA4qCI4qCQ4qCS4qKC4qCk4qO44qO/4qO/4qO/4qO/4qO/4qO/4qO/4qO/4qOf4qC/4qO/4qO/4qO/4qO/4qO/4qO/4qO/4qO/4qO/4qO/4qO/4qO/4qO/4qO/4qO/4qO/4qO/4qO/CuKjv+Kjv+Kjv+Kjv+Kjv+Kjv+Kjv+Kjv+Kjv+Kjv+Kjv+Kjv+Kjv+KhhOKggOKggOKggOKggOKggOKggOKghuKiuOKjv+KjvuKjv+Kjv+Kjv+Kjt+Kjn+KjveKjv+Kjn+Kjv+Kjv+Kju+Kjv+Kjv+Kjv+Kjv+Kjv+Kjv+Kjv+Kjv+Kjv+Kjv+Kjv+Kjv+Kjv+Kjv+Kjvwrio7/io7/io7/io7/io7/io7/io7/io7/io7/io7/io7/io7/io7/io7/io4bioIDioYDiooDioKDioZjio6Tio7/io7/ioJnio7/io7/ior/io7/io7/io7/io7/io7/io7/io7/io7fioKDiobvior/io7/io7/io7/io7/io7/io7/io7/io7/io7/io7/io7/io78K4qO/4qO/4qO/4qO/4qO/4qO/4qO/4qO/4qO/4qO/4qO/4qO/4qO/4qO/4qO/4qOn4qGA4qKC4qC14qO44qK34qG/4qO/4qO34qOE4qCK4qOu4qC74qC/4qO/4qO/4qOv4qK/4qO/4qCD4qKg4qOn4qKY4qGZ4qC/4qO/4qO/4qO/4qO/4qO/4qO/4qO/4qO/4qO/4qO/CuKjv+Kjv+Kjv+Kjv+Kjv+Kjv+Kjv+Kjv+Kjv+Kjv+Kjv+Kjv+Kjv+Kjv+Kjv+Kjv+Kjt+Khu+KjuOKgreKhq+Khv+KhveKju+Kjv+Kjt+KjhOKgm+Kgm+KgpuKhkeKgreKjpeKjtOKjvuKgt+KggeKjmOKhh+KiluKjlOKiruKijeKhueKjv+Kiv+Kjv+Kjv+Kjv+Kjvwrio7/io7/io7/io7/io7/io7/io7/io7/io7/io7/io7/io7/io7/io7/io7/io7/io7/io7/io5/ioJfioojioLPioZnioZbioavioJ7io7niopPioKTio4Dio6Tio6TiooDioIDioIDioIDioJjioJHioJLioJLioJLioInioKDioLzioIjioJnioLnio7/io7/io78K4qO/4qO/4qO/4qO/4qO/4qO/4qO/4qO/4qO/4qO/4qO/4qO/4qO/4qO/4qO/4qO/4qO/4qO/4qO/4qO34qOs4qCD4qCR4qCY4qKh4qCI4qCA4qCA4qCI4qCB4qCI4qCZ4qCI4qCA4qCA4qCA4qKg4qOk4qGE4qCA4qCA4qCA4qOk4qO04qGW4qO24qK64qO94qO/4qO/CuKiv+Khn+Kir+Khn+KjveKjq+Kin+Kjv+Kjv+Kjv+Kjv+Kjv+Kjv+Kjv+Kjv+Kjv+Kjv+Kjv+Kjv+Kjv+Kjv+Kjv+Kjp+KjhOKhgeKggOKggOKggOKhgOKigOKgoOKigOKjoOKjpOKjvuKjv+Kjv+Kjv+Kjv+Kjv+Kjv+Kjv+Kjv+Kjv+Kjn+Kih+Khr+Kjs+Kgu+KigAriopHioo7iobPio53ioqfio5vio67io7/io7/io7/io7/io7/io7/ior/io7/iob/io7/io7/io7/io7/iob/ioL/ioLvioI/ioonio4Dio7TioJviobLioZ/io7Xio7/ior/ioL/ioL/ioL/ioL/ioL/ioJ/ioL/ioJvioJvioJnioJPioIjioJLioIDioYDio4DioYAK4qOa4qKu4qG14qO+4qO74qO/4qO+4qO/4qO/4qO/4qO/4qO/4qOe4qO34qOu4qOz4qO54qOO4qO/4qOP4qCh4qGA4qCB4qCA4qCB4qCI4qCB4qCA4qCC4qCA4qCA4qCA4qCA4qCA4qCA4qCA4qCA4qCA4qCA4qKI4qCA4qCQ4qCA4qCA4qCA4qCQ4qCA4qCI4qCA4qCBCuKjv+Kjv+Kjv+Kjv+Kjv+Kjv+Kjv+Kjv+Kjv+Kjv+Kjv+Kjv+Kjv+Kjv+Kjv+Kjv+Kjv+Kjv+Kjv+Kjv+Kjv+KjhOKggOKggOKggOKghOKggOKggOKggOKggOKggOKggOKggOKggOKgsOKggOKggOKggOKggOKggOKggOKggOKggOKggOKggOKggOKggOKggOKggAogXyAgICAgICBfX18gX19fXwp8IHwgICAgIC8gKF8pIC8gL19fXyBfICAgICAgX18KfCB8IC98IC8gLyAvIC8gLyBfXyBcIHwgL3wgLyAvCnwgfC8gfC8gLyAvIC8gLyAvXy8gLyB8LyB8LyAvCnxfXy98X18vXy9fL18vXF9fX18vfF9fL3xfXy8gICAgICAgICAgICAgICAgICAgICAgICAgIA=="

      - path: /usr/local/bin/do-lvm-combine.sh
        permissions: '0755'
        content: |
          #!/bin/bash
          set -euo pipefail
          exec >> /var/log/do-lvm-combine.log 2>&1
          echo "$(date -Is): triggered"

          VG_NAME="${var.namespace}-vg"
          LV_NAME="${var.namespace}-lv"
          MOUNT_POINT="${var.lvm_mount_point}"
          FSTYPE="${var.lvm_filesystem}"
          DEVICE_GLOB="/dev/disk/by-id/scsi-0DO_Volume_${local.volume_name_prefix}*"

          shopt -s nullglob
          DEVICES=($DEVICE_GLOB)
          shopt -u nullglob

          if [ $${#DEVICES[@]} -eq 0 ]; then
            echo "no matching volumes present yet, exiting"
            exit 0
          fi

          for dev in "$${DEVICES[@]}"; do
            pvs "$dev" &>/dev/null || pvcreate -y "$dev"
          done

          if vgs "$VG_NAME" &>/dev/null; then
            for dev in "$${DEVICES[@]}"; do
              pvs -o vg_name --noheadings "$dev" | grep -q "$VG_NAME" || vgextend "$VG_NAME" "$dev"
            done
          else
            vgcreate "$VG_NAME" "$${DEVICES[@]}"
          fi

          if lvs "/dev/$VG_NAME/$LV_NAME" &>/dev/null; then
            lvextend -l +100%FREE "/dev/$VG_NAME/$LV_NAME" || true
            if [ "$FSTYPE" = "xfs" ]; then
              xfs_growfs "$MOUNT_POINT"
            else
              resize2fs "/dev/$VG_NAME/$LV_NAME"
            fi
          else
            lvcreate -l 100%FREE -n "$LV_NAME" "$VG_NAME"
            mkfs.$FSTYPE "/dev/$VG_NAME/$LV_NAME"
            mkdir -p "$MOUNT_POINT"
            grep -q "$VG_NAME-$LV_NAME" /etc/fstab || \
              echo "/dev/mapper/$${VG_NAME//-/--}-$${LV_NAME//-/--}  $MOUNT_POINT  $FSTYPE  defaults,nofail,discard  0  2" >> /etc/fstab
            mount "$MOUNT_POINT"
          fi

      - path: /etc/udev/rules.d/99-do-volume-combine.rules
        permissions: '0644'
        content: |
          ACTION=="add", SUBSYSTEM=="block", KERNEL=="sd*", ENV{ID_SERIAL}=="*DO_Volume_${local.volume_name_prefix}*", RUN+="/bin/systemd-run --no-block /usr/local/bin/do-lvm-combine.sh"

    runcmd:
      - chown -R ${var.user_name}:${var.user_name} /home/${var.user_name}
      - echo 'cat /etc/welcome_message && echo -e "\n\n"' >> /home/${var.user_name}/.bashrc
      - udevadm control --reload-rules
      - udevadm trigger --subsystem-match=block
      - /usr/local/bin/do-lvm-combine.sh
      - systemctl restart ssh
    EOF
}

locals {
  bucket_names = [for b in var.buckets : b.bucket_name]
  volume_name_prefix = "${var.namespace}-${random_string.suffix.result}-${var.name}-volume"
}

resource "random_string" "suffix" {
  length  = 6
  lower   = true
  upper   = false
  numeric = true
  special = false
}
