#!/bin/bash
# Systematisk testing av alle QEMU output-metoder

set +e
WORK_DIR="$(pwd)"

echo "=========================================="
echo "  SYSTEMATISK QEMU OUTPUT TESTING"
echo "=========================================="
echo ""

# Test 1: Chardev med explicit file backend
echo "[Test 1/5] Chardev file backend..."
MSYS_NO_PATHCONV=1 timeout 12 docker run --rm \
  -v "${WORK_DIR}:/work" \
  ubuntu:22.04 bash -c '
    apt-get update -qq && apt-get install -y -qq qemu-system-x86 > /dev/null 2>&1

    timeout 10 qemu-system-x86_64 \
      -drive file=/work/working-boot.img,format=raw,if=ide \
      -chardev file,id=char0,path=/work/test1-output.txt \
      -serial chardev:char0 \
      -m 512M \
      -display none \
      -no-reboot \
      2>&1 || true
' || true

if [ -f test1-output.txt ] && [ -s test1-output.txt ]; then
    echo "✓ Success! $(wc -c < test1-output.txt) bytes"
    head -10 test1-output.txt
else
    echo "✗ Ingen output"
fi
echo ""

# Test 2: Multiple serial ports
echo "[Test 2/5] Doble serieporter..."
MSYS_NO_PATHCONV=1 timeout 12 docker run --rm \
  -v "${WORK_DIR}:/work" \
  ubuntu:22.04 bash -c '
    apt-get update -qq && apt-get install -y -qq qemu-system-x86 > /dev/null 2>&1

    timeout 10 qemu-system-x86_64 \
      -drive file=/work/working-boot.img,format=raw,if=ide \
      -serial file:/work/test2-com1.txt \
      -serial file:/work/test2-com2.txt \
      -m 512M \
      -display none \
      -no-reboot \
      2>&1 || true
' || true

if [ -f test2-com1.txt ] && [ -s test2-com1.txt ]; then
    echo "✓ COM1: $(wc -c < test2-com1.txt) bytes"
    head -10 test2-com1.txt
elif [ -f test2-com2.txt ] && [ -s test2-com2.txt ]; then
    echo "✓ COM2: $(wc -c < test2-com2.txt) bytes"
    head -10 test2-com2.txt
else
    echo "✗ Ingen output"
fi
echo ""

# Test 3: VGA text mode output
echo "[Test 3/5] VGA text mode..."
MSYS_NO_PATHCONV=1 timeout 12 docker run --rm \
  -v "${WORK_DIR}:/work" \
  ubuntu:22.04 bash -c '
    apt-get update -qq && apt-get install -y -qq qemu-system-x86 > /dev/null 2>&1

    timeout 10 qemu-system-x86_64 \
      -drive file=/work/working-boot.img,format=raw,if=ide \
      -serial file:/work/test3-serial.txt \
      -vga std \
      -display none \
      -m 512M \
      -no-reboot \
      2>&1 > /work/test3-console.txt || true
' || true

echo "Console output:"
cat test3-console.txt 2>/dev/null | head -10 || echo "(tom)"
if [ -f test3-serial.txt ] && [ -s test3-serial.txt ]; then
    echo "✓ Serial: $(wc -c < test3-serial.txt) bytes"
    head -10 test3-serial.txt
else
    echo "✗ Ingen serial output"
fi
echo ""

# Test 4: Debugcon port (Bochs debug output)
echo "[Test 4/5] Debugcon port..."
MSYS_NO_PATHCONV=1 timeout 12 docker run --rm \
  -v "${WORK_DIR}:/work" \
  ubuntu:22.04 bash -c '
    apt-get update -qq && apt-get install -y -qq qemu-system-x86 > /dev/null 2>&1

    timeout 10 qemu-system-x86_64 \
      -drive file=/work/working-boot.img,format=raw,if=ide \
      -debugcon file:/work/test4-debugcon.txt \
      -global isa-debugcon.iobase=0x402 \
      -serial file:/work/test4-serial.txt \
      -m 512M \
      -display none \
      -no-reboot \
      2>&1 || true
' || true

if [ -f test4-debugcon.txt ] && [ -s test4-debugcon.txt ]; then
    echo "✓ Debugcon: $(wc -c < test4-debugcon.txt) bytes"
    head -10 test4-debugcon.txt
fi
if [ -f test4-serial.txt ] && [ -s test4-serial.txt ]; then
    echo "✓ Serial: $(wc -c < test4-serial.txt) bytes"
    head -10 test4-serial.txt
fi
if [ ! -f test4-debugcon.txt ] || [ ! -s test4-debugcon.txt ]; then
    if [ ! -f test4-serial.txt ] || [ ! -s test4-serial.txt ]; then
        echo "✗ Ingen output"
    fi
fi
echo ""

# Test 5: Direct stdout/stderr capture
echo "[Test 5/5] Direct stdout capture..."
MSYS_NO_PATHCONV=1 timeout 12 docker run --rm \
  -v "${WORK_DIR}:/work" \
  ubuntu:22.04 bash -c '
    apt-get update -qq && apt-get install -y -qq qemu-system-x86 > /dev/null 2>&1

    timeout 10 qemu-system-x86_64 \
      -drive file=/work/working-boot.img,format=raw,if=ide \
      -serial file:/work/test5-output.txt \
      -m 512M \
      -nographic \
      -no-reboot 2>&1
' > test5-stdout.log 2>&1 || true

if [ -f test5-output.txt ] && [ -s test5-output.txt ]; then
    echo "✓ Serial file: $(wc -c < test5-output.txt) bytes"
    head -10 test5-output.txt
fi
if [ -f test5-stdout.log ] && [ -s test5-stdout.log ]; then
    echo "Stdout/stderr: $(wc -c < test5-stdout.log) bytes"
    cat test5-stdout.log | head -10
else
    echo "✗ Ingen output"
fi

echo ""
echo "=========================================="
echo "OPPSUMMERING"
echo "=========================================="
echo ""

SUCCESS=0
for i in {1..5}; do
    FOUND=0
    if [ -f "test${i}-output.txt" ] && [ -s "test${i}-output.txt" ]; then
        SIZE=$(wc -c < "test${i}-output.txt")
        echo "✓ Test $i: SUCCESS ($SIZE bytes - test${i}-output.txt)"
        SUCCESS=$((SUCCESS + 1))
        FOUND=1
    fi
    if [ -f "test${i}-com1.txt" ] && [ -s "test${i}-com1.txt" ]; then
        SIZE=$(wc -c < "test${i}-com1.txt")
        echo "✓ Test $i: SUCCESS ($SIZE bytes - COM1)"
        SUCCESS=$((SUCCESS + 1))
        FOUND=1
    fi
    if [ -f "test${i}-serial.txt" ] && [ -s "test${i}-serial.txt" ]; then
        SIZE=$(wc -c < "test${i}-serial.txt")
        echo "✓ Test $i: SUCCESS ($SIZE bytes - serial.txt)"
        SUCCESS=$((SUCCESS + 1))
        FOUND=1
    fi
    if [ -f "test${i}-debugcon.txt" ] && [ -s "test${i}-debugcon.txt" ]; then
        SIZE=$(wc -c < "test${i}-debugcon.txt")
        echo "✓ Test $i: SUCCESS ($SIZE bytes - debugcon.txt)"
        SUCCESS=$((SUCCESS + 1))
        FOUND=1
    fi
    if [ $FOUND -eq 0 ]; then
        echo "✗ Test $i: Ingen output"
    fi
done

echo ""
echo "Vellykkede tester: $SUCCESS"
echo ""

# Vis første vellykket output
if [ $SUCCESS -gt 0 ]; then
    echo "=========================================="
    echo "FØRSTE VELLYKKET OUTPUT:"
    echo "=========================================="
    for i in {1..5}; do
        for file in "test${i}-output.txt" "test${i}-com1.txt" "test${i}-serial.txt" "test${i}-debugcon.txt"; do
            if [ -f "$file" ] && [ -s "$file" ]; then
                echo ""
                echo "=== $file ==="
                cat "$file"
                exit 0
            fi
        done
    done
fi
