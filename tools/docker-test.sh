#!/bin/bash
# Docker-based boot test for Folkering OS kernel

set -e

echo "Building kernel..."
cargo build --target x86_64-unknown-none

echo "Copying kernel to ISO root..."
cp target/x86_64-unknown-none/debug/kernel iso_root/boot/kernel.elf

echo "Building Docker test image..."
docker build -t folkering-test -f Dockerfile.test .

echo "Creating bootable disk image..."
docker run --rm -v "$(pwd):/test" -w /test ubuntu:22.04 bash -c "
    set -e
    apt-get update -qq && apt-get install -y -qq xorriso mtools dosfstools > /dev/null 2>&1

    # Create a 100MB disk image
    dd if=/dev/zero of=boot.img bs=1M count=100 2>/dev/null

    # Format as FAT32
    mkfs.fat -F 32 boot.img > /dev/null

    # Create temp mount point
    mkdir -p /mnt/disk
    mount -o loop boot.img /mnt/disk

    # Copy boot files
    cp -r iso_root/* /mnt/disk/

    # Unmount
    umount /mnt/disk
    rmdir /mnt/disk

    echo 'Disk image created successfully'
"

echo ""
echo "Running QEMU boot test..."
echo "========================================"
echo ""

docker run --rm -it \
    -v "$(pwd):/test" \
    -w /test \
    folkering-test \
    -drive file=boot.img,format=raw,if=virtio \
    -serial stdio \
    -no-reboot \
    -no-shutdown \
    -m 512M \
    -cpu qemu64 \
    -smp 1 \
    -display none

echo ""
echo "========================================"
echo "Test complete"
