#!/bin/bash
# Direct kernel test - bypass bootloader to test kernel execution

set -e

WORK_DIR="$(pwd)"

echo "=========================================="
echo "  DIRECT KERNEL BOOT TEST"
echo "=========================================="
echo ""

echo "[1/2] Building kernel..."
cd kernel
cargo build --target x86_64-folkering.json --release 2>&1 | grep -E "(Compiling|Finished|error|warning:)" || true
cd ..

echo ""
echo "[2/2] Testing kernel with QEMU (direct boot)..."
echo "Using -kernel option to bypass bootloader"
echo ""

# Use Docker to run QEMU (for Windows compatibility)
MSYS_NO_PATHCONV=1 docker run --rm \
  -v "${WORK_DIR}:/work" \
  -w //work \
  ubuntu:22.04 bash -c '
    apt-get update -qq && apt-get install -y -qq qemu-system-x86 2>&1 | grep -v "^debconf" || true

    timeout 10 qemu-system-x86_64 \
      -kernel /work/kernel/target/x86_64-folkering/release/kernel \
      -serial file:/work/direct-serial.log \
      -display none \
      -m 512M \
      -no-reboot \
      -no-shutdown \
      -d cpu_reset,guest_errors \
      -D /work/direct-debug.log \
      2>&1 | tee /work/direct-output.log || true
' 2>&1 | grep -v "^debconf" || true

echo ""
echo "=========================================="
echo "Test complete - checking output..."
echo "=========================================="
echo ""

if [ -f direct-serial.log ]; then
    echo "=== Serial output (direct-serial.log) ==="
    cat direct-serial.log
    echo ""
fi

if [ -f direct-debug.log ]; then
    echo "=== Debug log (last 20 lines) ==="
    tail -20 direct-debug.log
fi

echo ""
echo "Test files created:"
ls -lh direct-*.log 2>/dev/null || echo "No log files found"
