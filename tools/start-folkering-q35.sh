#!/bin/bash
# Folkering OS — Q35 + Intel IOMMU boot configuration
# Enables VT-d for DMA isolation of WASM drivers
#
# Differences from standard boot:
# - Machine type: q35 (ICH9 chipset, PCIe native)
# - Intel IOMMU device enabled
# - TCG only (WHPX incompatible with kernel-irqchip=split)
# - Single CPU (SMP needs kernel-irqchip=split with IOMMU)

FOLKDIR="$(cd "$(dirname "$0")/.." && pwd)"
SERIAL_LOG="$HOME/folkering-mcp/serial.log"

qemu-system-x86_64 \
  -machine q35 \
  -device intel-iommu \
  -drive "file=$FOLKDIR/boot/current.img,format=raw,if=ide" \
  -drive "file=$FOLKDIR/boot/virtio-data.img,format=raw,if=none,id=vdisk0" \
  -device virtio-blk-pci,drive=vdisk0 \
  -netdev user,id=net0 -device virtio-net-pci,netdev=net0 \
  -vga virtio -usb -device usb-tablet \
  -accel tcg \
  -cpu max,rdrand=on \
  -smp 1 -m 512M \
  -serial "file:$SERIAL_LOG" \
  -serial tcp:127.0.0.1:4567,server,nowait \
  -serial tcp:127.0.0.1:4568,server,nowait \
  -display none -vnc 0.0.0.0:0 -no-reboot

# Verified: boots successfully with DMAR detection
# [ACPI] DMAR found! VT-d available
# [ACPI]   Host address width: 47, flags: 0x1
# [ACPI]   DRHD: base=0xfed90000 segment=0 flags=0x0
