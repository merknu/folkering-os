#!/bin/bash
# Quick boot test - creates minimal boot image and tests in QEMU
set -e

echo "=== Quick Boot Test ==="
echo ""

# Build kernel
echo "[1/4] Building kernel..."
cd kernel
cargo build --target x86_64-folkering.json --release > /dev/null 2>&1
cd ..
echo "    ✓ Kernel built ($(ls -lh kernel/target/x86_64-folkering/release/kernel | awk '{print $5}'))"
echo ""

#  Use Docker to create boot image and test
echo "[2/4] Creating boot image with Docker..."
MSYS_NO_PATHCONV=1 docker run --rm -v "$(pwd):/work" ubuntu:22.04 bash -c '
set -e
export DEBIAN_FRONTEND=noninteractive

# Install tools
apt-get update -qq > /dev/null 2>&1
apt-get install -y -qq mtools dosfstools git build-essential nasm > /dev/null 2>&1

# Create 50MB FAT32 image (simpler than partitioned disk)
dd if=/dev/zero of=/work/boot.img bs=1M count=50 status=none
mkfs.fat -F 32 /work/boot.img > /dev/null 2>&1

# Copy kernel
export MTOOLS_SKIP_CHECK=1
mmd -i /work/boot.img ::/boot
mcopy -i /work/boot.img /work/kernel/target/x86_64-folkering/release/kernel ::/boot/kernel.elf

# Get Limine
cd /tmp
git clone https://github.com/limine-bootloader/limine.git --branch=v8.x --depth=1 > /dev/null 2>&1
cd limine
make > /dev/null 2>&1

# Copy Limine files
mcopy -i /work/boot.img limine-bios.sys ::/boot/
mcopy -i /work/boot.img /work/boot/limine.conf ::/

echo "✓ Boot image created"
'

echo ""
echo "[3/4] Verifying boot image..."
MSYS_NO_PATHCONV=1 docker run --rm -v "$(pwd):/work" ubuntu:22.04 bash -c '
apt-get update -qq && apt-get install -y -qq mtools > /dev/null 2>&1
export MTOOLS_SKIP_CHECK=1
echo "Root:"
mdir -i /work/boot.img ::
echo ""
echo "/boot:"
mdir -i /work/boot.img ::/boot
'

echo ""
echo "[4/4] Booting in QEMU (10 second timeout)..."
echo "========================================"
MSYS_NO_PATHCONV=1 docker run --rm -v "$(pwd):/work" ubuntu:22.04 bash -c '
apt-get update -qq && apt-get install -y -qq qemu-system-x86 > /dev/null 2>&1
timeout 10 qemu-system-x86_64 \
  -drive file=/work/boot.img,format=raw \
  -serial stdio \
  -m 512M \
  -display none \
  -no-reboot 2>&1 || true
'

echo ""
echo "========================================"
echo "Boot test complete!"
