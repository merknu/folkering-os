#!/bin/bash
export DEBIAN_FRONTEND=noninteractive
apt-get update -qq && apt-get install -y -qq mtools > /dev/null 2>&1

export MTOOLS_SKIP_CHECK=1

echo "Copying files to /tmp/boot.img..."

# Copy limine.conf to root
mcopy -i /tmp/boot.img limine.conf ::

# Create boot directory
mmd -i /tmp/boot.img ::/boot 2>/dev/null || true

# Copy kernel
mcopy -i /tmp/boot.img iso_root/boot/kernel.elf ::/boot/

# Copy limine-bios.sys
mcopy -i /tmp/boot.img iso_root/boot/limine-bios.sys ::/boot/

echo ""
echo "Files copied successfully!"
echo ""
echo "Root directory:"
mdir -i /tmp/boot.img ::
echo ""
echo "Boot directory:"
mdir -i /tmp/boot.img ::/boot
