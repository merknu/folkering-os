#!/bin/bash
# Create a bootable disk image for Folkering OS

set -e

echo "Building kernel..."
cargo build --target x86_64-unknown-none

echo "Copying kernel..."
cp target/x86_64-unknown-none/debug/kernel iso_root/boot/kernel.elf

echo "Creating disk image (100MB)..."
dd if=/dev/zero of=folkering.img bs=1M count=100

echo "Creating FAT32 filesystem..."
mkfs.fat -F 32 folkering.img

echo "Mounting image..."
mkdir -p mnt
sudo mount -o loop folkering.img mnt

echo "Copying boot files..."
sudo cp -r iso_root/* mnt/

echo "Installing Limine bootloader..."
sudo ./limine/limine-install folkering.img

echo "Unmounting..."
sudo umount mnt
rmdir mnt

echo "Boot image created: folkering.img"
echo ""
echo "To test with QEMU:"
echo "  qemu-system-x86_64 -drive file=folkering.img,format=raw -serial stdio -m 512M"
