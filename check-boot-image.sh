#!/bin/bash
set -e

echo "=== Checking Boot Image ==="
echo ""

# Install tools if needed
export DEBIAN_FRONTEND=noninteractive
apt-get update -qq > /dev/null 2>&1
apt-get install -y -qq mtools util-linux file > /dev/null 2>&1

# Check partition table
echo "1. Partition Table:"
fdisk -l /work/boot.img 2>&1 | grep -E "(Disk /work|boot.img|Device|Boot)"
echo ""

# Check MBR signature
echo "2. MBR Boot Signature:"
xxd -s 510 -l 2 /work/boot.img
echo ""

# Check FAT filesystem
echo "3. FAT Filesystem Check:"
export MTOOLS_SKIP_CHECK=1
minfo -i /work/boot.img@@1M :: 2>&1 || echo "FAT check failed"
echo ""

# List root directory
echo "4. Root Directory:"
mdir -i /work/boot.img@@1M :: 2>&1
echo ""

# List boot directory
echo "5. Boot Directory:"
mdir -i /work/boot.img@@1M ::/boot 2>&1
echo ""

# Check file sizes
echo "6. File Sizes:"
mdir -/ -i /work/boot.img@@1M ::/boot 2>&1 | tail -10
echo ""

echo "=== Check Complete ==="
