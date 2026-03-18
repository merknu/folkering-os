#!/bin/bash
# qemu-debug-live.sh — Start QEMU with QMP + GDB stub for live debugging
#
# QMP socket:  /tmp/folkering-qmp.sock   (for register inspection)
# GDB stub:    localhost:1234            (for breakpoints / single-step)
# Serial:      /tmp/folkering-serial.log (throttled by MCP tool)
#
# Usage (from WSL):
#   chmod +x tools/qemu-debug-live.sh
#   ./tools/qemu-debug-live.sh [disk-image]
#
# Usage (from Windows PowerShell/cmd):
#   wsl -e bash -c "cd /mnt/c/Users/merkn/folkering/folkering-os && ./tools/qemu-debug-live.sh"

set -e

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_DIR="$(dirname "$SCRIPT_DIR")"

# Default to most recent boot image
DISK_IMAGE="${1:-$PROJECT_DIR/boot/folkering.img}"

# Fallbacks
if [ ! -f "$DISK_IMAGE" ]; then
    for candidate in \
        "$PROJECT_DIR/working-boot.img" \
        "$PROJECT_DIR/phase8-working.img" \
        "$PROJECT_DIR/boot-copy.img"
    do
        if [ -f "$candidate" ]; then
            DISK_IMAGE="$candidate"
            break
        fi
    done
fi

if [ ! -f "$DISK_IMAGE" ]; then
    echo "ERROR: No disk image found. Build one first or pass path as argument."
    exit 1
fi

QMP_SOCK="/tmp/folkering-qmp.sock"
SERIAL_LOG="/tmp/folkering-serial.log"
GDB_PORT=1234

# Remove stale socket
rm -f "$QMP_SOCK"

echo "════════════════════════════════════════════════════"
echo "  Folkering OS — Live Debug Session"
echo "════════════════════════════════════════════════════"
echo "  Disk:    $DISK_IMAGE"
echo "  QMP:     $QMP_SOCK"
echo "  Serial:  $SERIAL_LOG"
echo "  GDB:     localhost:$GDB_PORT (QEMU halted — press g to continue)"
echo ""
echo "  MCP tools now available:"
echo "    qemu_inspect_registers  — read GPR/XMM state"
echo "    serial_throttle_analyzer $SERIAL_LOG"
echo "════════════════════════════════════════════════════"
echo ""

exec qemu-system-x86_64 \
    -drive file="$DISK_IMAGE",format=raw,if=ide \
    -m 512M \
    -serial file:"$SERIAL_LOG" \
    -display none \
    -no-reboot \
    -qmp unix:"$QMP_SOCK",server,nowait \
    -gdb tcp::$GDB_PORT \
    -S \
    "$@"
