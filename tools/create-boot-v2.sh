#!/bin/bash
set -e  # Exit on any error

echo "=== Folkering OS Boot Image Creation (Version 2) ==="
echo ""

# Install required tools
echo "[1/8] Installing tools..."
export DEBIAN_FRONTEND=noninteractive
apt-get update -qq
apt-get install -y -qq mtools fdisk dosfstools

# Remove old image if exists
rm -f /work/boot.img

# Create disk image (100MB)
echo "[2/8] Creating 100MB disk image..."
dd if=/dev/zero of=/work/boot.img bs=1M count=100 status=none

# Create partition table with sfdisk
echo "[3/8] Creating MBR partition table..."
cat <<EOF | sfdisk /work/boot.img
label: dos
start=2048, type=0x0C, bootable
EOF

# Format the partition using loop device emulation
echo "[4/8] Formatting partition as FAT32..."
# Calculate partition offset (2048 sectors * 512 bytes = 1048576 = 1M)
export MTOOLS_SKIP_CHECK=1
mformat -i /work/boot.img@@1M -F -v BOOT ::

# Verify FAT filesystem was created
echo "[5/8] Verifying FAT filesystem..."
minfo -i /work/boot.img@@1M :: || (echo "FAT verification failed!" && exit 1)

# Create boot directory
echo "[6/8] Creating /boot directory..."
mmd -i /work/boot.img@@1M ::/boot 2>/dev/null || true

# Copy files
echo "[7/8] Copying boot files..."
mcopy -i /work/boot.img@@1M /work/kernel.elf ::/boot/kernel.elf
mcopy -i /work/boot.img@@1M /work/limine-bios.sys ::/boot/limine-bios.sys
mcopy -i /work/boot.img@@1M /work/limine.conf ::/limine.conf

# Verify files were copied
echo "[8/8] Verifying files..."
echo ""
echo "Root directory:"
mdir -i /work/boot.img@@1M ::
echo ""
echo "Boot directory:"
mdir -i /work/boot.img@@1M ::/boot

echo ""
echo "=== Boot image created successfully! ==="
echo "File: /work/boot.img (100 MB)"
echo ""
echo "Note: Limine bootloader installation needs to be done separately"
echo "as it requires compiling Limine from source."
