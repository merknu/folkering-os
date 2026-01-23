#!/bin/bash
set -e

echo "=== Installing Limine Bootloader ==="
echo ""

# Install build tools
echo "[1/5] Installing build tools..."
export DEBIAN_FRONTEND=noninteractive
apt-get update -qq > /dev/null 2>&1
apt-get install -y -qq curl build-essential mtools > /dev/null 2>&1

# Download Limine
echo "[2/5] Downloading Limine v8.7.0..."
cd /tmp
if [ ! -f "limine-8.7.0.tar.gz" ]; then
    curl -sL -o limine-8.7.0.tar.gz https://github.com/limine-bootloader/limine/releases/download/v8.7.0/limine-8.7.0.tar.gz
fi

echo "[3/5] Extracting Limine..."
rm -rf limine-8.7.0
tar xzf limine-8.7.0.tar.gz
cd limine-8.7.0

# Build only the installer (not the full bootloader)
echo "[4/5] Building Limine installer..."
./configure --enable-bios-bin > /dev/null 2>&1
make limine > /dev/null 2>&1

# Install Limine to MBR
echo "[5/5] Installing bootloader to MBR..."
./limine bios-install /work/boot.img

# Verify installation
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
