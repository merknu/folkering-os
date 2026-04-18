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
#   1. tar+ssh tools/a64-streamer + tools/a64-encoder to ~/folkering-build/
#      (we use tar instead of rsync so this works from Windows Git Bash too,
#      where rsync isn't installed by default)
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

echo "[deploy] step 1/5: ship source via tar+ssh"
ssh "$PI" 'mkdir -p ~/folkering-build' >/dev/null
TARBALL="/tmp/folkering-deploy-$$.tar.gz"
trap 'rm -f "$TARBALL"' EXIT
tar -C "$REPO_ROOT/tools" \
    --exclude='*/target' --exclude='*.exe' --exclude='*.pdb' \
    --exclude='*/__pycache__' --exclude='*.pyc' \
    -czf "$TARBALL" a64-streamer a64-encoder
ssh "$PI" 'rm -rf ~/folkering-build/a64-streamer ~/folkering-build/a64-encoder'
ssh "$PI" 'tar -C ~/folkering-build -xzf -' < "$TARBALL"

echo "[deploy] step 2/5: build on Pi"
ssh "$PI" 'cd ~/folkering-build/a64-streamer && source ~/.cargo/env && cargo build --release --bin a64-stream-daemon 2>&1 | tail -1'

echo "[deploy] step 3/5: install systemd unit"
SERVICE_LOCAL="$REPO_ROOT/tools/a64-streamer/a64-stream-daemon.service"
scp -q "$SERVICE_LOCAL" "$PI:/tmp/a64-stream-daemon.service"
ssh "$PI" 'sudo install -m 644 /tmp/a64-stream-daemon.service /etc/systemd/system/a64-stream-daemon.service && sudo systemctl daemon-reload && sudo systemctl enable a64-stream-daemon'

echo "[deploy] step 4/5: restart service"
# Kill any orphan daemons left over from manual `nohup` runs (during
# debugging it's easy to leave one bound to :7700, then systemd
# startup fails with EADDRINUSE).
ssh "$PI" 'sudo pkill -9 -f a64-stream-daemon 2>/dev/null; sleep 1; sudo systemctl restart a64-stream-daemon'

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
