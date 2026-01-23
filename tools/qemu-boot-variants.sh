#!/bin/bash
# Test different QEMU boot configurations

set -e

WORK_DIR="$(pwd)"

echo "=========================================="
echo "  QEMU BOOT CONFIGURATION TESTS"
echo "=========================================="
echo ""

# Test 1: Using -drive with explicit boot index
echo "[Test 1] -drive with boot index..."
MSYS_NO_PATHCONV=1 timeout 10 docker run --rm \
  -v "${WORK_DIR}:/work" \
  ubuntu:22.04 bash -c '
    apt-get update -qq && apt-get install -y -qq qemu-system-x86 > /dev/null 2>&1

    timeout 8 qemu-system-x86_64 \
      -drive file=/work/bootable.img,format=raw,index=0,media=disk,if=ide \
      -boot order=c \
      -serial file:/work/test1.log \
      -m 512M \
      -display none \
      -no-reboot \
      2>&1 | grep -i "boot" || true
' 2>&1 | head -10 || true

if [ -f test1.log ] && [ -s test1.log ]; then
    echo "✓ Test 1: Got serial output"
    head -5 test1.log
else
    echo "✗ Test 1: No output"
fi
echo ""

# Test 2: Using -hda (simple hard disk)
echo "[Test 2] -hda simple interface..."
MSYS_NO_PATHCONV=1 timeout 10 docker run --rm \
  -v "${WORK_DIR}:/work" \
  ubuntu:22.04 bash -c '
    apt-get update -qq && apt-get install -y -qq qemu-system-x86 > /dev/null 2>&1

    timeout 8 qemu-system-x86_64 \
      -hda /work/bootable.img \
      -boot c \
      -serial file:/work/test2.log \
      -m 512M \
      -display none \
      -no-reboot \
      2>&1 | grep -i "boot" || true
' 2>&1 | head -10 || true

if [ -f test2.log ] && [ -s test2.log ]; then
    echo "✓ Test 2: Got serial output"
    head -5 test2.log
else
    echo "✗ Test 2: No output"
fi
echo ""

# Test 3: With VGA output captured
echo "[Test 3] With VGA text mode capture..."
MSYS_NO_PATHCONV=1 timeout 10 docker run --rm \
  -v "${WORK_DIR}:/work" \
  ubuntu:22.04 bash -c '
    apt-get update -qq && apt-get install -y -qq qemu-system-x86 > /dev/null 2>&1

    timeout 8 qemu-system-x86_64 \
      -hda /work/bootable.img \
      -serial file:/work/test3-serial.log \
      -monitor file:/work/test3-monitor.log \
      -m 512M \
      -vga std \
      -display none \
      -no-reboot \
      2>&1
' > test3-console.log 2>&1 || true

echo "Console output:"
cat test3-console.log 2>/dev/null | tail -10 || echo "(empty)"

if [ -f test3-serial.log ] && [ -s test3-serial.log ]; then
    echo "✓ Test 3: Got serial output"
    head -5 test3-serial.log
else
    echo "✗ Test 3: No serial output"
fi
echo ""

# Test 4: With BIOS debug
echo "[Test 4] With BIOS debug info..."
MSYS_NO_PATHCONV=1 timeout 10 docker run --rm \
  -v "${WORK_DIR}:/work" \
  ubuntu:22.04 bash -c '
    apt-get update -qq && apt-get install -y -qq qemu-system-x86 > /dev/null 2>&1

    timeout 8 qemu-system-x86_64 \
      -hda /work/bootable.img \
      -serial file:/work/test4.log \
      -m 512M \
      -display none \
      -no-reboot \
      -d int,cpu_reset \
      -D /work/test4-debug.log \
      2>&1
' > /dev/null 2>&1 || true

if [ -f test4.log ] && [ -s test4.log ]; then
    echo "✓ Test 4: Got serial output"
    head -5 test4.log
else
    echo "✗ Test 4: No output"
    echo "Debug log interrupts:"
    grep -i "int 0x" test4-debug.log 2>/dev/null | head -5 || echo "(no interrupts logged)"
fi
echo ""

echo "=========================================="
echo "Summary:"
echo "=========================================="
for i in 1 2 3 4; do
    if [ -f "test${i}.log" ] && [ -s "test${i}.log" ]; then
        echo "Test $i: SUCCESS ($(wc -c < test${i}.log) bytes)"
    elif [ -f "test${i}-serial.log" ] && [ -s "test${i}-serial.log" ]; then
        echo "Test $i: SUCCESS ($(wc -c < test${i}-serial.log) bytes)"
    else
        echo "Test $i: NO OUTPUT"
    fi
done
