#!/bin/bash
# Create a bootable image using only mtools (no sudo required)

set -e
WORKDIR=/home/knut/folkboot-work
PROJECT=/mnt/c/Users/merkn/folkering/folkering-os
OUTPUT="$PROJECT/folk-boot.img"

mkdir -p "$WORKDIR"
cd "$WORKDIR"

echo "=== Creating bootable image ==="

# Create 64MB disk image
dd if=/dev/zero of=disk.img bs=1M count=64 status=none

# Create MBR partition table using sfdisk
echo "Creating MBR partition table..."
echo "label: dos
start=2048, type=0c, bootable" | sfdisk disk.img > /dev/null 2>&1

# Calculate partition offset (2048 sectors * 512 bytes)
OFFSET=$((2048 * 512))

# Format the partition using mtools
echo "Formatting FAT32 partition..."
mformat -i disk.img@@$OFFSET -F -v FOLKERING ::

# Create boot directory
echo "Creating directories..."
mmd -i disk.img@@$OFFSET ::/boot

# Copy boot files
echo "Copying kernel..."
mcopy -i disk.img@@$OFFSET "$PROJECT/boot/kernel.elf" ::/boot/kernel.elf

echo "Copying initrd..."
mcopy -i disk.img@@$OFFSET "$PROJECT/boot/initrd.fpk" ::/boot/initrd.fpk

echo "Copying limine config..."
mcopy -i disk.img@@$OFFSET "$PROJECT/boot/limine.conf" ::/limine.conf

echo "Copying limine-bios.sys..."
mcopy -i disk.img@@$OFFSET "$PROJECT/boot/limine/bin/limine-bios.sys" ::/boot/limine-bios.sys

# Install Limine bootloader
echo "Installing Limine..."
"$PROJECT/boot/limine/bin/limine" bios-install disk.img

# Verify
echo "Verifying..."
mdir -i disk.img@@$OFFSET ::/boot/

# Copy to output
cp disk.img "$OUTPUT"
rm -f disk.img

echo "=== Done: $OUTPUT ==="
ls -la "$OUTPUT"
