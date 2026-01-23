#!/bin/bash
set -e

echo "=== Testing Folkering OS Kernel (Direct Boot) ==="
echo ""

# Make sure QEMU is installed
if ! command -v qemu-system-x86_64 &> /dev/null; then
    echo "Installing QEMU..."
    export DEBIAN_FRONTEND=noninteractive
    apt-get update -qq > /dev/null 2>&1
    apt-get install -y -qq qemu-system-x86 > /dev/null 2>&1
    echo "QEMU installed"
fi

echo "Kernel: /work/kernel.elf"
echo ""
echo "Starting QEMU with direct kernel boot (30 second timeout)..."
echo "==============================================================="
echo ""

# Boot kernel directly (bypassing bootloader)
timeout 30 qemu-system-x86_64 \
  -kernel /work/kernel.elf \
  -serial stdio \
  -m 512M \
  -nographic \
  -no-reboot \
  -monitor none \
  -d cpu_reset,guest_errors \
  2>&1

EXIT_CODE=$?
echo ""
echo "==============================================================="
echo "QEMU exited with code: $EXIT_CODE"
if [ $EXIT_CODE -eq 124 ]; then
    echo "(Timeout after 30 seconds - kernel running)"
elif [ $EXIT_CODE -eq 0 ]; then
    echo "(Clean exit)"
else
    echo "(Error or crash - exit code $EXIT_CODE)"
fi
