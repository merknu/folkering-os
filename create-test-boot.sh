#!/bin/bash
set -e

echo "=== Folkering OS Boot Image Creation ==="
echo ""

# Install required tools
echo "[1/7] Installing tools..."
export DEBIAN_FRONTEND=noninteractive
apt-get update -qq
apt-get install -y -qq mtools util-linux qemu-system-x86

# Create disk image
echo "[2/7] Creating 100MB disk image..."
dd if=/dev/zero of=/work/boot.img bs=1M count=100 status=none

# Create partition table
echo "[3/7] Creating partition table..."
echo "label: dos
start=2048, type=83, bootable" | sfdisk /work/boot.img > /dev/null 2>&1

# Format partition as FAT32
echo "[4/7] Formatting FAT32 filesystem..."
export MTOOLS_SKIP_CHECK=1
mformat -i /work/boot.img@@1M -F -v BOOT :: > /dev/null 2>&1

# Create boot directory and copy files
echo "[5/7] Copying boot files..."
mmd -i /work/boot.img@@1M ::/boot > /dev/null 2>&1 || true
mcopy -i /work/boot.img@@1M /work/kernel.elf ::/boot/ > /dev/null 2>&1
mcopy -i /work/boot.img@@1M /work/limine-bios.sys ::/boot/ > /dev/null 2>&1
mcopy -i /work/boot.img@@1M /work/limine.conf :: > /dev/null 2>&1

# Install Limine bootloader
echo "[6/7] Installing Limine bootloader..."
# Need to download and compile limine installer
apt-get install -y -qq git build-essential nasm mtools > /dev/null 2>&1
cd /tmp
git clone https://github.com/limine-bootloader/limine.git --branch=v8.x --depth=1 > /dev/null 2>&1
cd limine
make > /dev/null 2>&1
./limine bios-install /work/boot.img > /dev/null 2>&1

echo "[7/7] Boot image created successfully!"
echo ""
echo "Boot image: /work/boot.img (100 MB)"
echo ""
echo "Directory listing:"
mdir -i /work/boot.img@@1M ::
echo ""
mdir -i /work/boot.img@@1M ::/boot
echo ""
echo "=== Ready to test in QEMU ==="
