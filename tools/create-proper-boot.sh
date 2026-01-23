#!/bin/bash
# Create proper Limine boot image following official guidelines

set -e

WORK_DIR="$(pwd)"

echo "=========================================="
echo "  CREATING PROPER LIMINE BOOT IMAGE"
echo "=========================================="
echo ""

echo "[1/3] Building kernel..."
cd kernel
cargo build --target x86_64-folkering.json --release 2>&1 | grep -E "(Finished|error)" || true
cd ..
echo ""

echo "[2/3] Creating boot image with proper Limine structure..."
MSYS_NO_PATHCONV=1 docker run --rm \
  -v "${WORK_DIR}:/work" \
  ubuntu:22.04 bash -c '
    set -e
    export DEBIAN_FRONTEND=noninteractive

    echo "Installing tools..."
    apt-get update -qq > /dev/null 2>&1
    apt-get install -y -qq mtools parted git build-essential nasm dosfstools > /dev/null 2>&1

    echo "Creating 100MB disk image..."
    rm -f /work/folkering-boot.img
    dd if=/dev/zero of=/work/folkering-boot.img bs=1M count=100 status=none

    echo "Creating MBR partition table..."
    parted /work/folkering-boot.img -s mklabel msdos
    parted /work/folkering-boot.img -s mkpart primary fat32 2048s 100%
    parted /work/folkering-boot.img -s set 1 boot on

    echo "Formatting FAT32 filesystem..."
    # Calculate partition offset (2048 sectors * 512 bytes = 1048576 bytes = 1MB)
    LOOP_DEV=$(losetup -f)
    losetup -o 1048576 $LOOP_DEV /work/folkering-boot.img || true
    mkfs.fat -F 32 -n FOLKERING $LOOP_DEV 2>/dev/null || {
        # If loop device fails, use mtools
        export MTOOLS_SKIP_CHECK=1
        mformat -i /work/folkering-boot.img@@1M -F -v FOLKERING -N 0 ::
    }
    losetup -d $LOOP_DEV 2>/dev/null || true

    echo "Cloning Limine v8.x..."
    cd /tmp
    git clone https://github.com/limine-bootloader/limine.git --branch=v8.x --depth=1 > /dev/null 2>&1
    cd limine
    make > /dev/null 2>&1

    echo "Installing Limine bootloader to MBR..."
    ./limine bios-install /work/folkering-boot.img > /dev/null 2>&1

    echo "Copying files to boot partition..."
    export MTOOLS_SKIP_CHECK=1

    # Copy Limine stage-2 bootloader (MUST be in root for MBR to find it)
    mcopy -i /work/folkering-boot.img@@1M /tmp/limine/limine-bios.sys ::/

    # Copy Limine config
    mcopy -i /work/folkering-boot.img@@1M /work/boot/limine.conf ::/

    # Create boot directory and copy kernel
    mmd -i /work/folkering-boot.img@@1M ::/boot || true
    mcopy -i /work/folkering-boot.img@@1M /work/kernel/target/x86_64-folkering/release/kernel ::/boot/kernel.elf

    echo "✓ Boot image created: folkering-boot.img"
'
echo ""

echo "[3/3] Verifying boot image structure..."
MSYS_NO_PATHCONV=1 docker run --rm \
  -v "${WORK_DIR}:/work" \
  ubuntu:22.04 bash -c '
    apt-get update -qq && apt-get install -y -qq mtools file > /dev/null 2>&1
    export MTOOLS_SKIP_CHECK=1

    echo "File type:"
    file /work/folkering-boot.img

    echo ""
    echo "Root directory (/ - where Limine MBR looks):"
    mdir -i /work/folkering-boot.img@@1M :: 2>/dev/null

    echo ""
    echo "/boot directory (where kernel should be):"
    mdir -i /work/folkering-boot.img@@1M ::/boot 2>/dev/null
'
echo ""

echo "=========================================="
echo "✓ Boot image ready: folkering-boot.img"
echo "=========================================="
echo ""
echo "Testing boot in QEMU..."
echo ""

MSYS_NO_PATHCONV=1 timeout 15 docker run --rm \
  -v "${WORK_DIR}:/work" \
  ubuntu:22.04 bash -c '
    apt-get update -qq && apt-get install -y -qq qemu-system-x86 > /dev/null 2>&1

    timeout 12 qemu-system-x86_64 \
      -hda /work/folkering-boot.img \
      -serial stdio \
      -m 512M \
      -display none \
      -no-reboot \
      2>&1
' | tee folkering-boot-test.log || true

echo ""
echo "=========================================="
echo "Boot test complete"
echo "=========================================="
echo ""

if [ -f folkering-boot-test.log ] && [ -s folkering-boot-test.log ]; then
    echo "Output captured ($(wc -l < folkering-boot-test.log) lines):"
    cat folkering-boot-test.log
else
    echo "⚠ No output captured"
fi
