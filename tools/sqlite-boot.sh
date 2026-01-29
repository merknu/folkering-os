#!/bin/bash
# SQLite boot image test - includes initrd.fpk with SQLite database

set +e

WORK_DIR="$(pwd)"

echo "=========================================="
echo "  SQLITE BOOT IMAGE TEST"
echo "=========================================="
echo ""

echo "[1/6] Building kernel..."
cd kernel
cargo build --target x86_64-folkering.json --release 2>&1 | grep -E "(Compiling|Finished|warning|error)" | tail -5
cd ..
echo "Kernel: $(ls -lh kernel/target/x86_64-folkering/release/kernel 2>/dev/null | awk '{print $5}')"
echo ""

echo "[2/6] Building userspace..."
cd userspace
cargo build --target x86_64-folkering-userspace.json --release 2>&1 | grep -E "(Compiling|Finished|warning|error)" | tail -5
cd ..
echo "Synapse: $(ls -lh userspace/target/x86_64-folkering-userspace/release/synapse 2>/dev/null | awk '{print $5}')"
echo "Shell: $(ls -lh userspace/target/x86_64-folkering-userspace/release/shell 2>/dev/null | awk '{print $5}')"
echo ""

echo "[3/6] Creating SQLite database..."
echo "Hello from Folkering OS!" > boot/hello.txt
./tools/folk-pack/target/release/folk-pack create-sqlite boot/files.db \
  --add synapse:elf:userspace/target/x86_64-folkering-userspace/release/synapse \
  --add shell:elf:userspace/target/x86_64-folkering-userspace/release/shell \
  --add hello.txt:data:boot/hello.txt 2>&1 | tail -3
echo ""

echo "[4/6] Creating FPK initrd with SQLite database..."
./tools/folk-pack/target/release/folk-pack create boot/initrd.fpk \
  --add synapse:elf:userspace/target/x86_64-folkering-userspace/release/synapse \
  --add shell:elf:userspace/target/x86_64-folkering-userspace/release/shell \
  --add files.db:data:boot/files.db \
  --add hello.txt:data:boot/hello.txt 2>&1 | tail -3
echo "Initrd: $(ls -lh boot/initrd.fpk 2>/dev/null | awk '{print $5}')"
echo ""

echo "[5/6] Creating boot image with initrd..."
MSYS_NO_PATHCONV=1 docker run --rm \
  -v "${WORK_DIR}:/work" \
  ubuntu:22.04 bash -c '
    apt-get update -qq
    apt-get install -y -qq mtools fdisk 2>&1 | tail -1

    # Create 50MB disk
    dd if=/dev/zero of=/work/sqlite-boot.img bs=1M count=50 status=none

    # Create MBR with a single bootable partition
    echo "label: dos
    start=2048, type=0c, bootable" | sfdisk /work/sqlite-boot.img 2>&1 | grep -v "^$"

    export MTOOLS_SKIP_CHECK=1

    # Format partition
    mformat -i /work/sqlite-boot.img@@1M -F -v FOLKERING ::

    # Copy Limine stage-2 and config
    mcopy -i /work/sqlite-boot.img@@1M /work/boot/limine/bin/limine-bios.sys ::
    mcopy -i /work/sqlite-boot.img@@1M /work/boot/limine.conf ::

    # Create /boot and copy kernel + initrd
    mmd -i /work/sqlite-boot.img@@1M ::/boot
    mcopy -i /work/sqlite-boot.img@@1M /work/kernel/target/x86_64-folkering/release/kernel ::/boot/kernel.elf
    mcopy -i /work/sqlite-boot.img@@1M /work/boot/initrd.fpk ::/boot/initrd.fpk

    echo "Files copied:"
    mdir -i /work/sqlite-boot.img@@1M ::/boot 2>/dev/null | grep -E "(kernel|initrd)"
'

echo ""
echo "Installing Limine bootloader..."
MSYS_NO_PATHCONV=1 docker run --rm \
  -v "${WORK_DIR}:/work" \
  ubuntu:22.04 bash -c '
    /work/boot/limine/bin/limine bios-install /work/sqlite-boot.img 2>&1
'

echo ""
echo "[6/6] Booting in QEMU (30 second timeout)..."
echo ""

MSYS_NO_PATHCONV=1 timeout 30 docker run --rm \
  -v "${WORK_DIR}:/work" \
  ubuntu:22.04 bash -c '
    apt-get update -qq
    apt-get install -y -qq qemu-system-x86 2>&1 | tail -1

    qemu-system-x86_64 \
      -drive file=/work/sqlite-boot.img,format=raw,if=ide \
      -serial stdio \
      -m 512M \
      -display none \
      -no-reboot \
      -no-shutdown \
      2>&1
' | tee sqlite-boot-output.log || true

echo ""
echo "=========================================="
if [ -f sqlite-boot-output.log ] && [ -s sqlite-boot-output.log ]; then
    LINES=$(wc -l < sqlite-boot-output.log)
    echo "Boot output: $LINES lines"
    echo "=========================================="
    echo ""
    cat sqlite-boot-output.log
else
    echo "No boot output captured"
    echo "=========================================="
fi
