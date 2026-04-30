#!/bin/bash
# Folkering OS — Proxmox Nightly Build+Deploy+Test Pipeline
# Runs on the developer's Windows machine (Git Bash/WSL).
# Builds locally, deploys to Proxmox VM 900, runs health check.
#
# Usage: bash tools/nightly.sh
# Cron:  0 3 * * * cd /c/Users/merkn/folkering/folkering-os && bash tools/nightly.sh >> logs/nightly.log 2>&1

set -e
PROXMOX="root@192.168.68.150"
VMID=900
PROJECT="/c/Users/merkn/folkering/folkering-os"
TIMESTAMP=$(date +%Y%m%d-%H%M%S)

echo "=== Folkering OS Nightly Build — $TIMESTAMP ==="

# ── Step 1: Build ──
echo "[1/6] Building kernel..."
cd "$PROJECT/kernel" && cargo build --release 2>&1 | tail -1

echo "[2/6] Building userspace..."
cd "$PROJECT/userspace" && cargo build --release 2>&1 | tail -1

echo "[3/6] Packing initrd..."
cd "$PROJECT"
cp kernel/target/x86_64-folkering/release/kernel boot/iso_root/boot/kernel.elf
cargo run --manifest-path tools/folk-pack/Cargo.toml -- create boot/iso_root/boot/initrd.fpk \
  --add synapse:elf:userspace/target/x86_64-folkering-userspace/release/synapse \
  --add shell:elf:userspace/target/x86_64-folkering-userspace/release/shell \
  --add compositor:elf:userspace/target/x86_64-folkering-userspace/release/compositor \
  --add intent-service:elf:userspace/target/x86_64-folkering-userspace/release/intent-service \
  --add inference:elf:userspace/target/x86_64-folkering-userspace/release/inference \
  --add draug-streamer:elf:userspace/target/x86_64-folkering-userspace/release/draug-streamer \
  --add draug-daemon:elf:userspace/target/x86_64-folkering-userspace/release/draug-daemon 2>&1 | tail -1
py -3 tools/fat_inject.py 2>&1 | tail -1

# ── Step 2: Deploy ──
echo "[4/6] Deploying to Proxmox VM $VMID..."
scp -o StrictHostKeyChecking=no boot/current.img $PROXMOX:/var/lib/vz/images/$VMID/current.img 2>&1 | tail -1
ssh $PROXMOX "
  qm stop $VMID 2>/dev/null; sleep 3
  qm set $VMID --delete ide0 2>/dev/null
  # Clean up ALL old unused disks before importing
  for OLDDISK in \$(qm config $VMID | grep '^unused' | cut -d: -f1 | tr -d ' '); do
    qm set $VMID --delete \$OLDDISK 2>/dev/null
  done
  qm importdisk $VMID /var/lib/vz/images/$VMID/current.img local-lvm --format raw 2>&1 | tail -1
  DISK=\$(qm config $VMID | grep 'unused.*local-lvm' | tail -1 | cut -d: -f1 | tr -d ' ')
  DISKREF=\$(qm config $VMID | grep \"\$DISK\" | cut -d' ' -f2)
  qm set $VMID --ide0 \$DISKREF --boot order=ide0 2>&1 | tail -1
  > /tmp/folkering-serial.log
  qm start $VMID
  echo 'VM started'
" 2>&1

# ── Step 3: Wait for boot ──
echo "[5/6] Waiting 30s for boot..."
sleep 30

# ── Step 4: Health Check ──
echo "[6/6] Running health check..."
RESULT=$(ssh $PROXMOX "
  PASS=0; FAIL=0; LOG=/tmp/folkering-serial.log

  # Check DHCP
  if grep -q 'DHCP.*got' \$LOG; then echo 'PASS: DHCP'; PASS=\$((PASS+1))
  else echo 'FAIL: DHCP'; FAIL=\$((FAIL+1)); fi

  # Check Ping
  if grep -q 'Ping.*reply' \$LOG; then echo 'PASS: Ping'; PASS=\$((PASS+1))
  else echo 'FAIL: Ping'; FAIL=\$((FAIL+1)); fi

  # Check no panics
  if grep -q 'PANIC\|DOUBLE FAULT' \$LOG; then echo 'FAIL: PANIC detected'; FAIL=\$((FAIL+1))
  else echo 'PASS: No panics'; PASS=\$((PASS+1)); fi

  # Check compositor alive
  if grep -q 'LOOP ALIVE\|Omnibar ready' \$LOG; then echo 'PASS: Compositor'; PASS=\$((PASS+1))
  else echo 'FAIL: Compositor'; FAIL=\$((FAIL+1)); fi

  # Check Synapse
  if grep -q 'SYNAPSE.*Ready' \$LOG; then echo 'PASS: Synapse VFS'; PASS=\$((PASS+1))
  else echo 'FAIL: Synapse VFS'; FAIL=\$((FAIL+1)); fi

  # Check firewall loaded
  if grep -q 'IOAPIC\|firewall\|NET.*Stack' \$LOG; then echo 'PASS: Net stack'; PASS=\$((PASS+1))
  else echo 'FAIL: Net stack'; FAIL=\$((FAIL+1)); fi

  echo \"\"
  echo \"RESULT: \$PASS/\$((PASS+FAIL)) checks passed\"
  if [ \$FAIL -gt 0 ]; then exit 1; fi
" 2>&1)

echo "$RESULT"
echo ""

if echo "$RESULT" | grep -q "FAIL:"; then
  echo "=== NIGHTLY FAILED — $TIMESTAMP ==="
  exit 1
else
  echo "=== NIGHTLY PASSED — $TIMESTAMP ==="
  exit 0
fi
