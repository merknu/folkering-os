#!/bin/bash
# Working boot image with proper MBR partition

set +e

WORK_DIR="$(pwd)"

echo "=========================================="
echo "  WORKING BOOT IMAGE"
echo "=========================================="
echo ""

echo "[1/5] Building kernel..."
cd kernel
cargo build --target x86_64-folkering.json --release 2>&1 | grep "Finished"
cd ..
echo "✓ Kernel: $(ls -lh kernel/target/x86_64-folkering/release/kernel | awk '{print $5}')"
echo ""

echo "[2/5] Creating partitioned disk image..."
MSYS_NO_PATHCONV=1 docker run --rm \
  -v "${WORK_DIR}:/work" \
  ubuntu:22.04 bash -c '
    apt-get update -qq
    apt-get install -y -qq mtools fdisk 2>&1 | tail -1

    # Create 50MB disk
    dd if=/dev/zero of=/work/working-boot.img bs=1M count=50 status=none

    # Create MBR with a single bootable partition
    # Partition starts at sector 2048 (1MB offset)
    echo "label: dos
    start=2048, type=0c, bootable" | sfdisk /work/working-boot.img 2>&1 | grep -v "^$"

    echo "✓ Partition table created"
'

echo ""
echo "[3/5] Formatting partition and copying files..."
MSYS_NO_PATHCONV=1 docker run --rm \
  -v "${WORK_DIR}:/work" \
  ubuntu:22.04 bash -c '
    apt-get update -qq
    apt-get install -y -qq mtools 2>&1 | tail -1
    export MTOOLS_SKIP_CHECK=1

    # Format partition (offset = 2048 sectors * 512 bytes = 1048576 = 1M)
    mformat -i /work/working-boot.img@@1M -F -v FOLKERING ::

    # Copy Limine stage-2
    mcopy -i /work/working-boot.img@@1M /work/boot/limine/bin/limine-bios.sys ::

    # Copy config
    mcopy -i /work/working-boot.img@@1M /work/boot/limine.conf ::

    # Create /boot and copy kernel
    mmd -i /work/working-boot.img@@1M ::/boot
    mcopy -i /work/working-boot.img@@1M /work/kernel/target/x86_64-folkering/release/kernel ::/boot/kernel.elf

    echo "✓ Files copied"

    # Verify
    echo ""
    echo "Root directory:"
    mdir -i /work/working-boot.img@@1M :: 2>/dev/null | grep -v "^$"
    echo ""
    echo "/boot directory:"
    mdir -i /work/working-boot.img@@1M ::/boot 2>/dev/null | grep -v "^$"
'

echo ""
echo "[4/5] Installing Limine bootloader to MBR..."
MSYS_NO_PATHCONV=1 docker run --rm \
  -v "${WORK_DIR}:/work" \
  ubuntu:22.04 bash -c '
    /work/boot/limine/bin/limine bios-install /work/working-boot.img 2>&1
'

if [ $? -eq 0 ]; then
    echo "✓ Limine installed to MBR"
else
    echo "✗ Limine installation failed"
    exit 1
fi

echo ""
echo "[5/5] Testing boot in QEMU..."
echo "Timeout: 15 seconds"
echo ""

MSYS_NO_PATHCONV=1 timeout 15 docker run --rm \
  -v "${WORK_DIR}:/work" \
  ubuntu:22.04 bash -c '
    apt-get update -qq
    apt-get install -y -qq qemu-system-x86 2>&1 | tail -1

    qemu-system-x86_64 \
      -drive file=/work/working-boot.img,format=raw,if=ide \
      -serial stdio \
      -m 512M \
      -display none \
      -no-reboot \
      -no-shutdown \
      2>&1
' | tee working-boot-output.log || true

echo ""
echo "=========================================="
if [ -f working-boot-output.log ] && [ -s working-boot-output.log ]; then
    LINES=$(wc -l < working-boot-output.log)
    BYTES=$(wc -c < working-boot-output.log)
    echo "✓ Boot output captured: $LINES lines, $BYTES bytes"
    echo "=========================================="
    echo ""
    cat working-boot-output.log
else
    echo "✗ No boot output"
    echo "=========================================="
fi
