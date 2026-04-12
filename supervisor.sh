#!/bin/bash
# Folkering OS Supervisor — auto-restarts QEMU on crash.
# With boot persistence, Draug resumes from where it left off.
#
# Usage: bash supervisor.sh
# Stop:  Ctrl+C (kills QEMU and exits)

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PROXY_DIR="$HOME/folkering/folkering-proxy"
PROXY_LOG="$HOME/folkering-mcp/overnight_proxy.log"
SERIAL_LOG="$HOME/folkering-mcp/serial.log"

RESTART_COUNT=0
RESTART_DELAY=10

cleanup() {
    echo "[SUPERVISOR] Shutting down..."
    taskkill //F //IM qemu-system-x86_64.exe 2>/dev/null
    exit 0
}
trap cleanup INT TERM

# Ensure proxy is running
ensure_proxy() {
    if ! netstat -ano 2>/dev/null | grep -q ":14711.*LISTENING"; then
        echo "[SUPERVISOR] Proxy not running — starting..."
        taskkill //F //IM chrome.exe 2>/dev/null
        sleep 2
        cd "$PROXY_DIR" && nohup ./target/release/folkering-proxy.exe --backend chromium --port 14711 > "$PROXY_LOG" 2>&1 &
        sleep 6
        if netstat -ano 2>/dev/null | grep -q ":14711.*LISTENING"; then
            echo "[SUPERVISOR] Proxy started OK"
        else
            echo "[SUPERVISOR] WARNING: Proxy failed to start — will use mock fallback next time"
        fi
    fi
}

echo "[SUPERVISOR] Folkering OS auto-restart supervisor"
echo "[SUPERVISOR] Ctrl+C to stop"
echo ""

while true; do
    RESTART_COUNT=$((RESTART_COUNT + 1))
    echo "[SUPERVISOR] === Boot #$RESTART_COUNT === $(date)"

    # Ensure proxy is alive
    ensure_proxy

    # Start QEMU (folkering_run injects latest kernel + initrd)
    # Using the MCP tool is ideal but we can also call QEMU directly.
    # For now, just run QEMU and wait for it to exit.
    cd "$SCRIPT_DIR"

    # Truncate serial log for this boot (keep last 1000 lines as history)
    if [ -f "$SERIAL_LOG" ]; then
        tail -1000 "$SERIAL_LOG" > "${SERIAL_LOG}.prev"
        mv "${SERIAL_LOG}.prev" "$SERIAL_LOG"
    fi

    echo "[SUPERVISOR] Starting QEMU..."

    qemu-system-x86_64.exe \
        -drive "file=boot/current.img,format=raw,if=none,id=boot" \
        -device ide-hd,drive=boot \
        -drive "file=boot/virtio-data.img,format=raw,if=none,id=data" \
        -device virtio-blk-pci,drive=data \
        -m 2048 \
        -serial "file:$SERIAL_LOG" \
        -serial chardev:com2 \
        -chardev socket,id=com2,host=127.0.0.1,port=14711,nodelay=on \
        -device virtio-net-pci,netdev=net0 \
        -netdev user,id=net0,hostfwd=tcp::2222-:2222 \
        -device virtio-gpu-pci \
        -display vnc=:0 \
        -qmp tcp:127.0.0.1:4445,server,nowait \
        -accel whpx -accel tcg \
        2>"$HOME/folkering-mcp/qemu_stderr.log"

    EXIT_CODE=$?
    echo "[SUPERVISOR] QEMU exited (code=$EXIT_CODE) at $(date)"

    # Log last serial output for debugging
    echo "[SUPERVISOR] Last 5 serial lines:"
    tail -5 "$SERIAL_LOG" 2>/dev/null
    echo ""

    # Wait before restart
    echo "[SUPERVISOR] Restarting in ${RESTART_DELAY}s..."
    sleep $RESTART_DELAY
done
