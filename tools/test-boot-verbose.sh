#!/bin/bash
set +e  # Don't exit on error

echo "=== Testing Folkering OS Kernel Boot (Verbose) ==="
echo ""

# Make sure QEMU is installed
if ! command -v qemu-system-x86_64 &> /dev/null; then
    echo "Installing QEMU..."
    export DEBIAN_FRONTEND=noninteractive
    apt-get update -qq > /dev/null 2>&1
    apt-get install -y -qq qemu-system-x86 > /dev/null 2>&1
    echo "QEMU installed"
fi

echo "Boot image: /work/boot.img (100 MB)"
echo ""
echo "Starting QEMU with verbose output (30 second timeout)..."
echo "========================================================"
echo ""

# Run QEMU with more verbose options and longer timeout
timeout 30 qemu-system-x86_64 \
  -drive file=/work/boot.img,format=raw,if=ide \
  -serial stdio \
  -m 512M \
  -nographic \
  -no-reboot \
  -monitor none \
  -d cpu_reset,guest_errors \
  2>&1

EXIT_CODE=$?
echo ""
echo "========================================================"
echo "QEMU exited with code: $EXIT_CODE"
if [ $EXIT_CODE -eq 124 ]; then
    echo "(Timeout after 30 seconds)"
elif [ $EXIT_CODE -eq 0 ]; then
    echo "(Clean exit)"
else
    echo "(Error or crash - exit code $EXIT_CODE)"
fi
