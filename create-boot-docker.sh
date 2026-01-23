#!/bin/bash
# Create bootable disk using Docker

set -e

echo "Creating bootable disk with Docker..."

docker run --rm --privileged \
    -v "$(pwd):/work" \
    -v "/tmp/limine:/limine" \
    -w /work \
    ubuntu:22.04 bash -c '
        set -e
        export DEBIAN_FRONTEND=noninteractive

        echo "Installing dependencies..."
        apt-get update -qq && apt-get install -y -qq parted dosfstools > /dev/null 2>&1

        DISK="/work/folkering-boot.img"

        echo "Creating 100MB disk image..."
        dd if=/dev/zero of=$DISK bs=1M count=100 2>/dev/null

        echo "Creating partition table..."
        parted -s $DISK mklabel msdos
        parted -s $DISK mkpart primary fat32 1MiB 100%
        parted -s $DISK set 1 boot on

        echo "Setting up loop device..."
        LOOP=$(losetup -f --show $DISK)
        partprobe $LOOP

        echo "Formatting partition..."
        mkfs.fat -F 32 ${LOOP}p1 > /dev/null

        echo "Mounting partition..."
        mkdir -p /mnt/boot
        mount ${LOOP}p1 /mnt/boot

        echo "Copying files..."
        mkdir -p /mnt/boot/boot
        cp iso_root/boot/kernel.elf /mnt/boot/boot/
        cp limine.conf /mnt/boot/
        cp /limine/limine-bios.sys /mnt/boot/boot/

        echo "Unmounting..."
        umount /mnt/boot
        losetup -d $LOOP

        echo "Installing Limine bootloader..."
        cd /limine
        ./limine bios-install /work/$DISK

        echo ""
        echo "Success! Created: $DISK"
    '

echo ""
echo "Bootable disk created: folkering-boot.img"
