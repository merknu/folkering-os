#!/bin/bash
# Comprehensive boot debugging test

set -e

WORK_DIR="$(pwd)"

echo "=========================================="
echo "  FOLKERING OS - BOOT TEST"
echo "=========================================="
echo ""

# Clean up old logs
rm -f BOOT.log serial.log qemu-debug.log BOOT-OUTPUT.txt

# Write header to log file
cat > BOOT.log << 'EOF'
===========================================
  FOLKERING OS - BOOT TEST
===========================================

EOF

echo "[1/4] Rebuilding kernel..."
cd kernel
cargo build --target x86_64-folkering.json --release 2>&1 | grep -E "(Compiling|Finished|error)" || true
KERNEL_SIZE=$(ls -lh target/x86_64-folkering/release/kernel | awk '{print $5}')
echo "    ✓ Kernel: $KERNEL_SIZE"
cd ..
echo ""

echo "[2/4] Creating boot image..."
# Use existing bootable.img or create new one
if [ ! -f bootable.img ]; then
    echo "    Creating new bootable.img..."
    MSYS_NO_PATHCONV=1 docker run --rm \
      -v "${WORK_DIR}:/work" \
      ubuntu:22.04 bash -c '
        set -e
        apt-get update -qq && apt-get install -y -qq mtools parted git build-essential nasm > /dev/null 2>&1

        # Create 100MB disk
        dd if=/dev/zero of=/work/bootable.img bs=1M count=100 status=none

        # Create MBR partition
        parted /work/bootable.img -s mklabel msdos
        parted /work/bootable.img -s mkpart primary fat32 1MiB 100%
        parted /work/bootable.img -s set 1 boot on

        # Format FAT32
        export MTOOLS_SKIP_CHECK=1
        mformat -i /work/bootable.img@@1M -F -v BOOT ::

        # Copy kernel
        mmd -i /work/bootable.img@@1M ::/boot || true
        mcopy -i /work/bootable.img@@1M /work/kernel/target/x86_64-folkering/release/kernel ::/boot/kernel.elf

        # Install Limine
        cd /tmp
        git clone https://github.com/limine-bootloader/limine.git --branch=v8.x --depth=1 > /dev/null 2>&1
        cd limine && make > /dev/null 2>&1

        mcopy -i /work/bootable.img@@1M /tmp/limine/limine-bios.sys ::/boot/
        mcopy -i /work/bootable.img@@1M /work/boot/limine.conf ::/

        ./limine bios-install /work/bootable.img > /dev/null 2>&1
    '
else
    echo "    Using existing bootable.img"
    echo "    Updating kernel only..."
    MSYS_NO_PATHCONV=1 docker run --rm \
      -v "${WORK_DIR}:/work" \
      ubuntu:22.04 bash -c '
        apt-get update -qq && apt-get install -y -qq mtools > /dev/null 2>&1
        export MTOOLS_SKIP_CHECK=1
        mcopy -i /work/bootable.img@@1M -o /work/kernel/target/x86_64-folkering/release/kernel ::/boot/kernel.elf
    '
fi
echo "    ✓ Boot image ready"
echo ""

echo "[3/4] Verifying boot image..."
MSYS_NO_PATHCONV=1 docker run --rm \
  -v "${WORK_DIR}:/work" \
  ubuntu:22.04 bash -c '
    apt-get update -qq && apt-get install -y -qq mtools > /dev/null 2>&1
    export MTOOLS_SKIP_CHECK=1
    echo "Contents:"
    mdir -i /work/bootable.img@@1M :: 2>/dev/null
    mdir -i /work/bootable.img@@1M ::/boot 2>/dev/null
' | tee -a BOOT.log
echo ""

echo "[4/4] Booting in QEMU..."
echo "    Timeout: 20 seconds"
echo "    Serial log: serial.log"
echo "    Debug log: qemu-debug.log"
echo ""

# Run QEMU with comprehensive logging
MSYS_NO_PATHCONV=1 docker run --rm \
  -v "${WORK_DIR}:/work" \
  ubuntu:22.04 bash -c '
    apt-get update -qq && apt-get install -y -qq qemu-system-x86 > /dev/null 2>&1

    timeout 20 qemu-system-x86_64 \
      -drive file=/work/bootable.img,format=raw,if=ide,index=0,media=disk \
      -boot c \
      -serial file:/work/serial.log \
      -m 512M \
      -display none \
      -no-reboot \
      -no-shutdown \
      -cpu qemu64 \
      -d cpu_reset,guest_errors,int \
      -D /work/qemu-debug.log \
      2>&1 | tee /work/BOOT-OUTPUT.txt
' 2>&1 | tee -a BOOT.log || true

echo "" | tee -a BOOT.log
echo "qemu-system-x86_64: terminating on signal 15 from pid $$ (timeout)" >> BOOT.log
echo "" | tee -a BOOT.log

echo "=========================================" | tee -a BOOT.log
echo "Boot test complete" | tee -a BOOT.log
echo "=========================================" | tee -a BOOT.log
echo "" | tee -a BOOT.log

# Display results
echo "Results:"
echo ""

if [ -f serial.log ] && [ -s serial.log ]; then
    echo "=== Serial output (serial.log) ==="
    cat serial.log
    echo ""
else
    echo "⚠ Serial output is empty"
    echo ""
fi

if [ -f qemu-debug.log ]; then
    echo "=== QEMU debug log (first 30 lines) ==="
    head -30 qemu-debug.log
    echo ""
    if [ $(wc -l < qemu-debug.log) -gt 30 ]; then
        echo "... ($(wc -l < qemu-debug.log) total lines)"
        echo ""
    fi
fi

if [ -f BOOT-OUTPUT.txt ] && [ -s BOOT-OUTPUT.txt ]; then
    echo "=== QEMU output ==="
    cat BOOT-OUTPUT.txt
    echo ""
fi

echo "Log files:"
ls -lh BOOT.log serial.log qemu-debug.log BOOT-OUTPUT.txt 2>/dev/null || true
