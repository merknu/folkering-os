#!/bin/bash
# Quick Test Script for Folkering OS in WSL
# Run with: wsl -d Ubuntu-22.04 bash ~/folkering/kernel/test-in-wsl.sh

set -e  # Exit on error

echo "========================================="
echo "Folkering OS - WSL Test Script"
echo "========================================="
echo ""

# Colors
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
NC='\033[0m' # No Color

# Step 1: Install dependencies
echo -e "${YELLOW}[1/5] Checking dependencies...${NC}"
MISSING=""

if ! command -v qemu-system-x86_64 &> /dev/null; then
    MISSING="$MISSING qemu-system-x86"
fi

if ! command -v clang &> /dev/null; then
    MISSING="$MISSING clang"
fi

if ! command -v nasm &> /dev/null; then
    MISSING="$MISSING nasm"
fi

if ! command -v xorriso &> /dev/null; then
    MISSING="$MISSING xorriso"
fi

if ! command -v make &> /dev/null; then
    MISSING="$MISSING build-essential"
fi

if [ -n "$MISSING" ]; then
    echo -e "${YELLOW}Installing missing packages:$MISSING${NC}"
    sudo apt update
    sudo apt install -y $MISSING mtools
fi

echo -e "${GREEN}✓ All dependencies installed${NC}"

# Step 2: Build Limine
echo ""
echo -e "${YELLOW}[2/5] Building Limine bootloader...${NC}"
cd ~/folkering/kernel

if [ ! -f limine/limine-bios-cd.bin ]; then
    echo "Building Limine from source..."
    cd limine
    ./configure
    make -j$(nproc)
    cd ..
    echo -e "${GREEN}✓ Limine built successfully${NC}"
else
    echo -e "${GREEN}✓ Limine already built${NC}"
fi

# Step 3: Create ISO structure
echo ""
echo -e "${YELLOW}[3/5] Creating ISO structure...${NC}"

rm -rf iso_root
mkdir -p iso_root/boot/limine
mkdir -p iso_root/EFI/BOOT

# Copy kernel
cp target/x86_64-folkering/release/kernel iso_root/kernel
echo -e "${GREEN}✓ Kernel copied${NC}"

# Copy Limine config
cp limine.conf iso_root/boot/limine/
echo -e "${GREEN}✓ Configuration copied${NC}"

# Copy Limine binaries
cp limine/limine-bios.sys iso_root/boot/limine/ 2>/dev/null || echo -e "${YELLOW}  Warning: limine-bios.sys not found${NC}"
cp limine/limine-bios-cd.bin iso_root/boot/limine/
cp limine/limine-uefi-cd.bin iso_root/boot/limine/
cp limine/BOOTX64.EFI iso_root/EFI/BOOT/ 2>/dev/null || echo -e "${YELLOW}  Warning: BOOTX64.EFI not found (UEFI boot disabled)${NC}"
echo -e "${GREEN}✓ Bootloader binaries copied${NC}"

# Step 4: Create ISO
echo ""
echo -e "${YELLOW}[4/5] Creating ISO image...${NC}"

xorriso -as mkisofs \
    -b boot/limine/limine-bios-cd.bin \
    -no-emul-boot \
    -boot-load-size 4 \
    -boot-info-table \
    --efi-boot boot/limine/limine-uefi-cd.bin \
    -efi-boot-part \
    --efi-boot-image \
    --protective-msdos-label \
    iso_root \
    -o folkering.iso \
    2>&1 | grep -v "^xorriso" | head -10

# Install bootloader
if [ -f limine/limine ]; then
    ./limine/limine bios-install folkering.iso 2>/dev/null || true
fi

ISO_SIZE=$(du -h folkering.iso | cut -f1)
echo -e "${GREEN}✓ ISO created: folkering.iso ($ISO_SIZE)${NC}"

# Step 5: Boot test
echo ""
echo -e "${YELLOW}[5/5] Booting in QEMU...${NC}"
echo -e "${YELLOW}Press Ctrl+A then X to exit QEMU${NC}"
echo ""
sleep 2

# Run QEMU
qemu-system-x86_64 \
    -cdrom folkering.iso \
    -m 512M \
    -serial stdio \
    -no-reboot \
    -no-shutdown \
    -d cpu_reset \
    2>&1 | tee qemu-output.log

# Check result
echo ""
echo "========================================="
if grep -q "KERNEL PANIC" qemu-output.log; then
    echo -e "${RED}Boot failed - kernel panic detected${NC}"
    echo "Check qemu-output.log for details"
    exit 1
elif grep -q "Folkering" qemu-output.log; then
    echo -e "${GREEN}✓ Kernel booted successfully!${NC}"
    exit 0
else
    echo -e "${YELLOW}Boot status unknown - check qemu-output.log${NC}"
    exit 2
fi
