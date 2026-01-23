#!/bin/bash
set -e

echo "=== Installing GRUB Bootloader ==="
echo ""

# Install GRUB
echo "[1/4] Installing GRUB..."
export DEBIAN_FRONTEND=noninteractive
apt-get update -qq > /dev/null 2>&1
apt-get install -y -qq grub-pc-bin mtools > /dev/null 2>&1

# Create GRUB config
echo "[2/4] Creating GRUB configuration..."
mkdir -p /tmp/grub
cat > /tmp/grub/grub.cfg <<'EOF'
set timeout=0
set default=0

menuentry "Folkering OS" {
    multiboot2 /boot/kernel.elf
    boot
}
EOF

# Install GRUB to boot image
echo "[3/4] Installing GRUB to MBR..."
grub-install --target=i386-pc --boot-directory=/tmp/grub /work/boot.img

# Copy GRUB config to boot image
echo "[4/4] Copying GRUB configuration..."
mcopy -i /work/boot.img@@1M /tmp/grub/grub.cfg ::/boot/grub/

echo "✓ GRUB installed successfully!"
echo ""
echo "=== GRUB bootloader installed to /work/boot.img ==="
echo ""
echo "Ready to boot!"
