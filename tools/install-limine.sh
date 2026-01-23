#!/bin/bash
set -e

echo "=== Installing Limine Bootloader ==="
echo ""

# Install build tools
echo "[1/4] Installing build tools..."
export DEBIAN_FRONTEND=noninteractive
apt-get update -qq
apt-get install -y -qq git build-essential nasm mtools

# Clone and build Limine
echo "[2/4] Cloning and building Limine v8.x..."
cd /tmp
if [ ! -d "limine" ]; then
    git clone https://github.com/limine-bootloader/limine.git --branch=v8.x --depth=1 -q
fi
cd limine
make -j$(nproc) > /dev/null 2>&1

# Install Limine to MBR
echo "[3/4] Installing bootloader to MBR..."
./limine bios-install /work/boot.img

# Verify installation
echo "[4/4] Verifying bootloader installation..."
if [ $? -eq 0 ]; then
    echo "✓ Bootloader installed successfully!"
else
    echo "✗ Bootloader installation failed!"
    exit 1
fi

echo ""
echo "=== Limine bootloader installed to /work/boot.img ==="
echo ""
echo "Ready to boot!"
