#!/bin/bash
# Windows-compatible boot test script using Docker
# Works in Git Bash / MINGW64 environment

set -e

echo "======================================"
echo "  Folkering OS - Boot Test (Windows)"
echo "======================================"
echo ""

# Get absolute path in format Docker on Windows expects
# Convert /c/Users/... to //c/Users/... for Docker volume mounting
WORK_DIR="$(pwd | sed 's|^/c/|//c/|')"
echo "Working directory: $WORK_DIR"
echo ""

echo "[1/6] Building kernel (release mode)..."
cd kernel
cargo build --target x86_64-folkering.json --release
cd ..
echo "    ✓ Kernel built: $(ls -lh kernel/target/x86_64-folkering/release/kernel | awk '{print $5}')"
echo ""

echo "[2/6] Preparing boot files..."
# Copy kernel to boot directory
mkdir -p boot/files
cp kernel/target/x86_64-folkering/release/kernel boot/files/kernel.elf
echo "    ✓ Kernel copied to boot/files/"
echo ""

echo "[3/6] Creating boot image with Docker..."
docker run --rm \
  -v "${WORK_DIR}:/work" \
  -w //work \
  ubuntu:22.04 bash -c '
    set -e
    export DEBIAN_FRONTEND=noninteractive

    echo "    Installing tools..."
    apt-get update -qq > /dev/null 2>&1
    apt-get install -y -qq mtools util-linux git build-essential nasm > /dev/null 2>&1

    echo "    Creating 100MB disk image..."
    dd if=/dev/zero of=/work/boot.img bs=1M count=100 status=none

    echo "    Creating partition table..."
    echo "label: dos
start=2048, type=83, bootable" | sfdisk /work/boot.img > /dev/null 2>&1

    echo "    Formatting FAT32 filesystem..."
    export MTOOLS_SKIP_CHECK=1
    mformat -i /work/boot.img@@1M -F -v BOOT :: > /dev/null 2>&1

    echo "    Copying kernel..."
    mmd -i /work/boot.img@@1M ::/boot > /dev/null 2>&1 || true
    mcopy -i /work/boot.img@@1M /work/boot/files/kernel.elf ::/boot/ > /dev/null 2>&1

    echo "    Installing Limine v8.7.0..."
    cd /tmp
    git clone https://github.com/limine-bootloader/limine.git --branch=v8.x --depth=1 > /dev/null 2>&1
    cd limine
    make > /dev/null 2>&1

    echo "    Copying Limine files..."
    mcopy -i /work/boot.img@@1M /tmp/limine/limine-bios.sys ::/boot/ > /dev/null 2>&1
    mcopy -i /work/boot.img@@1M /work/boot/limine.conf :: > /dev/null 2>&1

    echo "    Installing bootloader to MBR..."
    ./limine bios-install /work/boot.img > /dev/null 2>&1

    echo "    ✓ Boot image created successfully!"
'
echo ""

echo "[4/6] Verifying boot image contents..."
docker run --rm \
  -v "${WORK_DIR}:/work" \
  -w //work \
  ubuntu:22.04 bash -c '
    apt-get update -qq > /dev/null 2>&1
    apt-get install -y -qq mtools > /dev/null 2>&1
    export MTOOLS_SKIP_CHECK=1
    echo "    Root directory:"
    mdir -i /work/boot.img@@1M :: 2>/dev/null || echo "    (empty)"
    echo "    /boot directory:"
    mdir -i /work/boot.img@@1M ::/boot 2>/dev/null || echo "    (empty)"
'
echo ""

echo "[5/6] Installing QEMU in Docker container..."
echo "    (This may take a minute on first run)"
docker run --rm \
  -v "${WORK_DIR}:/work" \
  -w //work \
  ubuntu:22.04 bash -c '
    export DEBIAN_FRONTEND=noninteractive
    apt-get update -qq > /dev/null 2>&1
    apt-get install -y -qq qemu-system-x86 > /dev/null 2>&1
    echo "    ✓ QEMU installed"
'
echo ""

echo "[6/6] Starting boot test in QEMU..."
echo "========================================"
echo ""
echo "Expected output:"
echo "  - Limine bootloader messages"
echo "  - Folkering OS banner"
echo "  - Phase 1-3 initialization"
echo "  - Task spawning messages"
echo "  - Scheduler start"
echo "  - IPC syscall logs"
echo ""
echo "Press Ctrl+C to exit QEMU"
echo "========================================"
echo ""

# Run QEMU with serial output (timeout after 30 seconds)
timeout 30 docker run --rm -it \
  -v "${WORK_DIR}:/work" \
  -w //work \
  ubuntu:22.04 bash -c '
    apt-get update -qq > /dev/null 2>&1
    apt-get install -y -qq qemu-system-x86 > /dev/null 2>&1

    qemu-system-x86_64 \
      -drive file=/work/boot.img,format=raw,if=ide \
      -serial stdio \
      -m 512M \
      -display none \
      -no-reboot \
      -cpu qemu64
' || true

echo ""
echo "========================================"
echo "Boot test complete!"
echo "========================================"
echo ""
echo "Boot image saved as: boot.img (100 MB)"
echo ""
echo "To run again:"
echo "  ./tools/test-windows.sh"
echo ""
echo "To run with custom timeout:"
echo "  timeout 60 ./tools/test-windows.sh"
echo ""
