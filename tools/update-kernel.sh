#!/bin/bash
# Quick script to update kernel in working-boot.img

set -e

echo "=== Updating Kernel in Boot Image ==="
echo ""

MSYS_NO_PATHCONV=1 docker run --rm \
    -v "$(pwd):/work" \
    -w /work \
    ubuntu:22.04 bash -c '
        set -e
        export DEBIAN_FRONTEND=noninteractive
        export MTOOLS_SKIP_CHECK=1

        echo "Installing mtools..."
        apt-get update -qq && apt-get install -y -qq mtools > /dev/null 2>&1

        echo "Copying new kernel to boot image..."
        mcopy -i working-boot.img@@1M -o kernel/target/x86_64-folkering/release/kernel ::/boot/kernel.elf

        echo ""
        echo "✓ Kernel updated successfully!"
        echo ""
        echo "Boot directory contents:"
        mdir -i working-boot.img@@1M ::/boot
    '

echo ""
echo "=== Done ==="
