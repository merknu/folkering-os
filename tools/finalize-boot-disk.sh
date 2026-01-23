#!/bin/bash
# Finalize boot disk by copying all necessary files

set -e

DISK_IMG="/tmp/boot.img"
KERNEL_DIR="/work"

echo "Installing mtools..."
apt-get update -qq && apt-get install -y -qq mtools > /dev/null 2>&1

export MTOOLS_SKIP_CHECK=1

echo "Copying limine.conf to root..."
mcopy -i $DISK_IMG $KERNEL_DIR/limine.conf ::

echo "Creating /boot directory..."
mmd -i $DISK_IMG ::/boot 2>/dev/null || true

echo "Copying kernel.elf..."
mcopy -i $DISK_IMG $KERNEL_DIR/iso_root/boot/kernel.elf ::/boot/

echo "Copying limine-bios.sys..."
mcopy -i $DISK_IMG $KERNEL_DIR/iso_root/boot/limine-bios.sys ::/boot/

echo ""
echo "✅ All files copied successfully!"
echo ""
echo "Boot disk contents:"
mdir -i $DISK_IMG ::
echo ""
echo "Boot directory:"
mdir -i $DISK_IMG ::/boot

echo ""
echo "Boot disk is ready at: $DISK_IMG"
