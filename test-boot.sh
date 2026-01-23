#!/bin/bash
set -e

echo "=== Testing Folkering OS Kernel Boot ==="
echo ""

# Make sure QEMU is installed
if ! command -v qemu-system-x86_64 &> /dev/null; then
    echo "Installing QEMU..."
    export DEBIAN_FRONTEND=noninteractive
    apt-get update -qq > /dev/null 2>&1
    apt-get install -y -qq qemu-system-x86 > /dev/null 2>&1
fi

echo "Boot image: /work/boot.img (100 MB)"
echo ""
echo "Starting QEMU..."
echo "=================="
echo ""

# Run QEMU with serial output
qemu-system-x86_64 \
  -drive file=/work/boot.img,format=raw,if=ide \
  -serial stdio \
  -m 512M \
  -display none \
  -no-reboot
