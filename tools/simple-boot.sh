#!/bin/bash
# Simple boot image creation using only mtools

set +e  # Don't exit on error, handle them manually

WORK_DIR="$(pwd)"

echo "=========================================="
echo "  SIMPLE BOOT IMAGE CREATION"
echo "=========================================="
echo ""

echo "Step 1: Building kernel..."
cd kernel
cargo build --target x86_64-folkering.json --release 2>&1 | grep "Finished" || echo "Build failed!"
cd ..

KERNEL_PATH="kernel/target/x86_64-folkering/release/kernel"
if [ ! -f "$KERNEL_PATH" ]; then
    echo "ERROR: Kernel not found at $KERNEL_PATH"
    exit 1
fi
KERNEL_SIZE=$(ls -lh "$KERNEL_PATH" | awk '{print $5}')
echo "✓ Kernel built: $KERNEL_SIZE"
echo ""

echo "Step 2: Creating boot image in Docker..."
MSYS_NO_PATHCONV=1 docker run --rm \
  -v "${WORK_DIR}:/work" \
  ubuntu:22.04 bash -c '
    set +e  # Dont exit on errors
    export DEBIAN_FRONTEND=noninteractive
    export MTOOLS_SKIP_CHECK=1

    echo "Installing packages..."
    apt-get update -qq
    apt-get install -y -qq mtools git build-essential nasm 2>&1 | tail -1

    echo "Creating 50MB disk..."
    dd if=/dev/zero of=/work/simple-boot.img bs=1M count=50 2>&1 | tail -1

    echo "Formatting as FAT32 (no partitions, whole disk)..."
    mformat -i /work/simple-boot.img -F -v FOLKERING :: 2>&1 | tail -1
    if [ $? -ne 0 ]; then
        echo "ERROR: mformat failed"
        exit 1
    fi

    echo "Verifying format..."
    mdir -i /work/simple-boot.img :: 2>&1 | head -3

    echo "Using pre-built Limine binaries from boot/limine/bin/..."
    LIMINE_BIN="/work/boot/limine/bin"

    if [ ! -f "$LIMINE_BIN/limine" ]; then
        echo "ERROR: Limine binaries not found at $LIMINE_BIN"
        exit 1
    fi

    echo "Installing Limine bootloader to MBR..."
    "$LIMINE_BIN/limine" bios-install /work/simple-boot.img 2>&1 | tail -1
    if [ $? -ne 0 ]; then
        echo "ERROR: Limine install failed"
        exit 1
    fi

    echo "Copying limine-bios.sys to root..."
    mcopy -i /work/simple-boot.img "$LIMINE_BIN/limine-bios.sys" :: 2>&1 | tail -1
    if [ $? -ne 0 ]; then
        echo "ERROR: Failed to copy limine-bios.sys"
        exit 1
    fi

    echo "Copying limine.conf..."
    mcopy -i /work/simple-boot.img /work/boot/limine.conf :: 2>&1 | tail -1
    if [ $? -ne 0 ]; then
        echo "ERROR: Failed to copy limine.conf"
        exit 1
    fi

    echo "Creating /boot directory..."
    mmd -i /work/simple-boot.img ::/boot 2>&1 | tail -1

    echo "Copying kernel..."
    mcopy -i /work/simple-boot.img /work/kernel/target/x86_64-folkering/release/kernel ::/boot/kernel.elf 2>&1 | tail -1
    if [ $? -ne 0 ]; then
        echo "ERROR: Failed to copy kernel"
        exit 1
    fi

    echo "✓ All files copied successfully"
    echo ""
    echo "=== Final structure ==="
    echo "Root:"
    mdir -i /work/simple-boot.img :: 2>&1
    echo ""
    echo "/boot:"
    mdir -i /work/simple-boot.img ::/boot 2>&1
'

if [ $? -eq 0 ]; then
    echo ""
    echo "✓ Boot image created: simple-boot.img"
else
    echo ""
    echo "✗ Boot image creation failed"
    exit 1
fi

echo ""
echo "Step 3: Testing in QEMU..."
echo ""

MSYS_NO_PATHCONV=1 timeout 15 docker run --rm \
  -v "${WORK_DIR}:/work" \
  ubuntu:22.04 bash -c '
    apt-get update -qq
    apt-get install -y -qq qemu-system-x86 2>&1 | tail -1

    timeout 12 qemu-system-x86_64 \
      -hda /work/simple-boot.img \
      -serial stdio \
      -m 512M \
      -display none \
      -no-reboot \
      2>&1
' | tee simple-boot-test.log || true

echo ""
echo "=========================================="
if [ -f simple-boot-test.log ] && [ -s simple-boot-test.log ]; then
    LINES=$(wc -l < simple-boot-test.log)
    echo "✓ Boot test complete - $LINES lines of output"
    echo "=========================================="
    echo ""
    cat simple-boot-test.log
else
    echo "✗ No output from QEMU"
    echo "=========================================="
fi
