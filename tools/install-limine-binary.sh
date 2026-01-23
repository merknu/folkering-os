#!/bin/bash
set -e

echo "=== Installing Limine Bootloader (Binary) ==="
echo ""

# Install tools
echo "[1/3] Installing required tools..."
export DEBIAN_FRONTEND=noninteractive
apt-get update -qq > /dev/null 2>&1
apt-get install -y -qq curl > /dev/null 2>&1

# Download Limine installer binary for Linux x86_64
echo "[2/3] Downloading Limine installer binary..."
cd /tmp
curl -sL -o limine-install https://github.com/limine-bootloader/limine/raw/v8.x-binary/limine-bios-install
chmod +x limine-install

# Install Limine to MBR
echo "[3/3] Installing bootloader to MBR..."
./limine-install /work/boot.img

# Verify installation
if [ $? -eq 0 ]; then
    echo "✓ Bootloader installed successfully!"
    echo ""
    echo "=== Limine bootloader installed to /work/boot.img ==="
    echo ""
    echo "Ready to boot!"
else
    echo "✗ Bootloader installation failed!"
    exit 1
fi
