#!/usr/bin/env bash
# Deploy + rebuild + restart `a64-stream-daemon` on the Pi.
#
# Replaces the manual ~12-step ssh+scp+pkill+nohup ritual we used to
# debug the worker bug with one idempotent script. Safe to re-run:
# rsync only ships changed bytes, cargo build is a no-op when source
# hasn't changed, systemctl restart is idempotent.
#
# Usage:
#   PI=knut@192.168.68.72 ./scripts/deploy-daemon-to-pi.sh
#   ./scripts/deploy-daemon-to-pi.sh                    # uses PI default
#
# What it does:
#   1. rsync tools/a64-streamer + tools/a64-encoder to ~/folkering-build/
#      (excluding target/, *.exe, *.pdb so we don't ship Windows builds)
#   2. cargo build --release on the Pi (only rebuilds if source changed)
#   3. Install/refresh /etc/systemd/system/a64-stream-daemon.service
#      (sudo, prompts for password the first time)
#   4. systemctl restart a64-stream-daemon
#   5. Quick smoke test — verify daemon is listening on :7700

set -euo pipefail

PI="${PI:-knut@192.168.68.72}"
HOST="${PI#*@}"
PORT="${PORT:-7700}"

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"

echo "[deploy] target: $PI"
echo "[deploy] repo:   $REPO_ROOT"
echo

echo "[deploy] step 1/5: rsync source"
ssh "$PI" 'mkdir -p ~/folkering-build' >/dev/null
rsync -az --delete --info=stats0 \
    --exclude target/ --exclude '*.exe' --exclude '*.pdb' \
    "$REPO_ROOT/tools/a64-streamer/" \
    "$PI:~/folkering-build/a64-streamer/"
rsync -az --delete --info=stats0 \
    --exclude target/ --exclude '*.exe' --exclude '*.pdb' \
    "$REPO_ROOT/tools/a64-encoder/" \
    "$PI:~/folkering-build/a64-encoder/"

echo "[deploy] step 2/5: build on Pi"
ssh "$PI" 'cd ~/folkering-build/a64-streamer && source ~/.cargo/env && cargo build --release --bin a64-stream-daemon 2>&1 | tail -1'

echo "[deploy] step 3/5: install systemd unit"
SERVICE_LOCAL="$REPO_ROOT/tools/a64-streamer/a64-stream-daemon.service"
scp -q "$SERVICE_LOCAL" "$PI:/tmp/a64-stream-daemon.service"
ssh "$PI" 'sudo install -m 644 /tmp/a64-stream-daemon.service /etc/systemd/system/a64-stream-daemon.service && sudo systemctl daemon-reload && sudo systemctl enable a64-stream-daemon'

echo "[deploy] step 4/5: restart service"
ssh "$PI" 'sudo systemctl restart a64-stream-daemon'

echo "[deploy] step 5/5: verify listening"
sleep 2
if ssh "$PI" "ss -ltn | grep -q ':$PORT '"; then
    echo "[deploy] OK — daemon listening on $HOST:$PORT"
    ssh "$PI" 'sudo journalctl -u a64-stream-daemon --no-pager -n 5'
else
    echo "[deploy] FAIL — daemon not listening on :$PORT" >&2
    ssh "$PI" 'sudo journalctl -u a64-stream-daemon --no-pager -n 20' >&2
    exit 1
fi
