#!/bin/bash
# Create a proper bootable disk image with Limine

set -e

DISK_IMG="/tmp/folkering-boot.img"
MOUNT_POINT="/tmp/folkering-mount"
LIMINE_DIR="/tmp/limine"
KERNEL_DIR="/mnt/c/Users/merkn/OneDrive/Dokumenter/Meray_vault/Meray/Projects/Folkering-OS/code/kernel"

echo "Creating disk image (100MB)..."
dd if=/dev/zero of=$DISK_IMG bs=1M count=100 2>/dev/null

echo "Creating partition table..."
# Create MBR partition table with one bootable FAT32 partition
parted -s $DISK_IMG mklabel msdos
parted -s $DISK_IMG mkpart primary fat32 1MiB 100%
parted -s $DISK_IMG set 1 boot on

echo "Setting up loop device..."
LOOP_DEV=$(sudo losetup -f --show $DISK_IMG)
sudo partprobe $LOOP_DEV

echo "Formatting partition..."
sudo mkfs.fat -F 32 ${LOOP_DEV}p1

echo "Mounting partition..."
sudo mkdir -p $MOUNT_POINT
sudo mount ${LOOP_DEV}p1 $MOUNT_POINT

echo "Creating boot directory structure..."
sudo mkdir -p $MOUNT_POINT/boot/limine

echo "Copying files..."
sudo cp $KERNEL_DIR/iso_root/boot/kernel.elf $MOUNT_POINT/boot/
sudo cp $KERNEL_DIR/limine.conf $MOUNT_POINT/
sudo cp $LIMINE_DIR/limine-bios.sys $MOUNT_POINT/boot/

echo "Unmounting..."
sudo umount $MOUNT_POINT
sudo losetup -d $LOOP_DEV
rmdir $MOUNT_POINT

echo "Installing Limine bootloader..."
cd $LIMINE_DIR
./limine bios-install $DISK_IMG

echo ""
echo "Bootable disk created: $DISK_IMG"
echo ""
echo "To test:"
echo "  qemu-system-x86_64 -drive file=$DISK_IMG,format=raw,if=ide -serial stdio -m 512M"
