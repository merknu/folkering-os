#!/bin/bash
# Build a 512 MB Folkering deploy image (large enough to hold a
# Q8-quantized Qwen3-0.6B fbin + the runtime initrd).
set -e
cd /mnt/c/Users/merkn/folkering/folkering-os
DEPLOY=boot/folkering-deploy-512m.img

echo "[1] truncate fresh 512MB..."
rm -f "$DEPLOY"
dd if=/dev/zero of="$DEPLOY" bs=1M count=512 status=none

echo "[2] partition table..."
parted -s "$DEPLOY" mklabel msdos
parted -s "$DEPLOY" mkpart primary fat32 1MiB 100%
parted -s "$DEPLOY" set 1 boot on
parted -s "$DEPLOY" set 1 lba on
parted -s "$DEPLOY" print | tail -3

echo "[3] mformat partition (offset 1048576)..."
mformat -i "$DEPLOY@@1048576" -F -v FOLKERING ::

echo "[4] mkdir + install limine..."
mmd -i "$DEPLOY@@1048576" ::/boot
mmd -i "$DEPLOY@@1048576" ::/boot/limine
mcopy -i "$DEPLOY@@1048576" boot/iso_root/boot/limine-bios.sys ::/boot/limine-bios.sys
mcopy -i "$DEPLOY@@1048576" boot/iso_root/boot/limine/limine.conf ::/boot/limine/limine.conf

echo "[5] limine bios-install..."
boot/limine/bin/limine bios-install "$DEPLOY"

ls -la "$DEPLOY"
