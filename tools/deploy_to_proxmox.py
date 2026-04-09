#!/usr/bin/env python3
"""Folkering OS — Proxmox Deployment Pipeline

Deploys Folkering OS to a Proxmox VE server for testing with proper KVM
(no WHPX DMA coherency issues). Uses SSH for file transfer and qm CLI
for VM management.

Usage:
  1. Build locally: kernel + userspace + fat_inject
  2. python tools/deploy_to_proxmox.py deploy
  3. python tools/deploy_to_proxmox.py serial  (monitor serial output)
  4. python tools/deploy_to_proxmox.py destroy  (cleanup)

Prerequisites:
  - SSH key access to Proxmox (pi_key or password)
  - Proxmox host: 192.168.68.150
  - VM ID 900 reserved for Folkering OS
"""

import subprocess
import sys
import os
import time
import argparse

# ── Configuration ────────────────────────────────────────────────────────

PROXMOX_HOST = "192.168.68.150"
PROXMOX_USER = "root"  # Proxmox root access for qm commands
PROXMOX_SSH_KEY = os.path.expanduser("~/.ssh/id_rsa")  # Try default key
VM_ID = 900
VM_NAME = "folkering-os"
VM_MEMORY = 512  # MB
VM_CORES = 4
STORAGE = "local"  # Proxmox storage name

# Local paths
PROJECT_DIR = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
BOOT_IMG = os.path.join(PROJECT_DIR, "boot", "current.img")
DATA_IMG = os.path.join(PROJECT_DIR, "boot", "virtio-data.img")

# Remote paths on Proxmox
REMOTE_DIR = "/var/lib/vz/images/900"
REMOTE_BOOT = f"{REMOTE_DIR}/current.img"
REMOTE_DATA = f"{REMOTE_DIR}/virtio-data.img"
REMOTE_SERIAL_LOG = "/tmp/folkering-serial.log"

# Fix Windows console
if sys.platform == "win32":
    sys.stdout.reconfigure(encoding="utf-8", errors="replace")
    sys.stderr.reconfigure(encoding="utf-8", errors="replace")


def ssh(cmd, check=True, capture=False):
    """Run command on Proxmox via SSH."""
    ssh_cmd = [
        "ssh", "-o", "StrictHostKeyChecking=no",
        "-o", "ConnectTimeout=5",
        f"{PROXMOX_USER}@{PROXMOX_HOST}",
        cmd
    ]
    # Try with key first
    if os.path.exists(PROXMOX_SSH_KEY):
        ssh_cmd.insert(1, "-i")
        ssh_cmd.insert(2, PROXMOX_SSH_KEY)

    if capture:
        result = subprocess.run(ssh_cmd, capture_output=True, text=True, timeout=30)
        return result.stdout.strip()
    else:
        result = subprocess.run(ssh_cmd, timeout=60)
        if check and result.returncode != 0:
            print(f"SSH command failed: {cmd}")
            return False
        return True


def scp_upload(local_path, remote_path):
    """Upload file to Proxmox via SCP."""
    scp_cmd = [
        "scp", "-o", "StrictHostKeyChecking=no",
    ]
    if os.path.exists(PROXMOX_SSH_KEY):
        scp_cmd.extend(["-i", PROXMOX_SSH_KEY])
    scp_cmd.extend([local_path, f"{PROXMOX_USER}@{PROXMOX_HOST}:{remote_path}"])

    print(f"  Uploading {os.path.basename(local_path)} ({os.path.getsize(local_path) // 1024}KB)...")
    result = subprocess.run(scp_cmd, timeout=300)
    return result.returncode == 0


def cmd_deploy():
    """Deploy Folkering OS to Proxmox."""
    print()
    print("=" * 60)
    print("  Folkering OS — Proxmox Deployment")
    print("=" * 60)
    print()

    # Check local files exist
    for f in [BOOT_IMG, DATA_IMG]:
        if not os.path.exists(f):
            print(f"ERROR: {f} not found. Build first!")
            return 1
        print(f"  Local: {os.path.basename(f)} ({os.path.getsize(f) // 1024}KB)")

    # Test SSH connectivity
    print(f"\n[1/5] Testing SSH to {PROXMOX_HOST}...")
    result = ssh("hostname", capture=True)
    if not result:
        print(f"  ERROR: Cannot SSH to {PROXMOX_HOST}")
        print(f"  Try: ssh {PROXMOX_USER}@{PROXMOX_HOST}")
        return 1
    print(f"  Connected to: {result}")

    # Create remote directory
    print(f"\n[2/5] Preparing remote storage...")
    ssh(f"mkdir -p {REMOTE_DIR}", check=False)

    # Upload images
    print(f"\n[3/5] Uploading boot images...")
    if not scp_upload(BOOT_IMG, REMOTE_BOOT):
        print("  ERROR: Failed to upload boot image")
        return 1
    if not scp_upload(DATA_IMG, REMOTE_DATA):
        print("  ERROR: Failed to upload data image")
        return 1
    print("  Upload complete!")

    # Destroy existing VM if any
    print(f"\n[4/5] Creating VM {VM_ID} ({VM_NAME})...")
    ssh(f"qm stop {VM_ID} 2>/dev/null; sleep 2; qm destroy {VM_ID} 2>/dev/null", check=False)

    # Create VM with:
    # - KVM acceleration (default on Proxmox)
    # - E1000 NIC with user-mode networking (SLIRP equivalent)
    # - VirtIO block for data disk
    # - VirtIO GPU
    # - Serial port as socket for monitoring
    # - VNC display
    vm_config = f"""qm create {VM_ID} \\
        --name {VM_NAME} \\
        --memory {VM_MEMORY} \\
        --cores {VM_CORES} \\
        --cpu host \\
        --bios ovmf \\
        --machine q35 \\
        --ide0 {REMOTE_BOOT},format=raw \\
        --virtio0 {REMOTE_DATA},format=raw \\
        --net0 e1000,bridge=vmbr0,macaddr=52:54:00:12:34:56 \\
        --vga virtio \\
        --serial0 socket \\
        --args '-serial file:{REMOTE_SERIAL_LOG}'"""

    # Actually, Proxmox uses different syntax. Let me use a simpler approach:
    # Create the VM with basic config, then set args manually
    create_cmd = (
        f"qm create {VM_ID}"
        f" --name {VM_NAME}"
        f" --memory {VM_MEMORY}"
        f" --cores {VM_CORES}"
        f" --cpu host"
        f" --ostype other"
        f" --ide0 {REMOTE_BOOT},format=raw,media=disk"
        f" --virtio0 {REMOTE_DATA},format=raw"
        f" --net0 e1000,bridge=vmbr0,macaddr=52:54:00:12:34:56"
        f" --vga std"
        f" --serial0 socket"
        f" --boot order=ide0"
    )

    if not ssh(create_cmd):
        print("  ERROR: Failed to create VM")
        # Try without some options
        simple_cmd = (
            f"qm create {VM_ID}"
            f" --name {VM_NAME}"
            f" --memory {VM_MEMORY}"
            f" --cores {VM_CORES}"
            f" --ostype other"
            f" --ide0 {REMOTE_BOOT},format=raw"
            f" --virtio0 {REMOTE_DATA},format=raw"
            f" --net0 e1000,bridge=vmbr0"
            f" --serial0 socket"
        )
        if not ssh(simple_cmd):
            print("  ERROR: Simple create also failed")
            return 1

    # Start VM
    print(f"\n[5/5] Starting VM {VM_ID}...")
    if not ssh(f"qm start {VM_ID}"):
        print("  ERROR: Failed to start VM")
        return 1

    print(f"\n  VM {VM_ID} is running on Proxmox!")
    print(f"  Serial console: ssh {PROXMOX_USER}@{PROXMOX_HOST} 'qm terminal {VM_ID}'")
    print(f"  VNC: Proxmox web UI → VM {VM_ID} → Console")
    print(f"  Serial log: {REMOTE_SERIAL_LOG}")
    print(f"\n  To monitor: python tools/deploy_to_proxmox.py serial")
    return 0


def cmd_serial():
    """Monitor serial output from Proxmox VM."""
    print(f"Monitoring serial output from VM {VM_ID}...")
    print(f"(Ctrl+C to stop)\n")

    # Use qm terminal for serial console
    ssh_cmd = [
        "ssh", "-o", "StrictHostKeyChecking=no", "-t",
        f"{PROXMOX_USER}@{PROXMOX_HOST}",
        f"qm terminal {VM_ID} -iface serial0"
    ]
    if os.path.exists(PROXMOX_SSH_KEY):
        ssh_cmd.insert(1, "-i")
        ssh_cmd.insert(2, PROXMOX_SSH_KEY)

    try:
        subprocess.run(ssh_cmd)
    except KeyboardInterrupt:
        print("\nDisconnected.")


def cmd_status():
    """Check VM status."""
    result = ssh(f"qm status {VM_ID}", capture=True)
    print(f"VM {VM_ID}: {result}")


def cmd_stop():
    """Stop the VM."""
    ssh(f"qm stop {VM_ID}")
    print(f"VM {VM_ID} stopped.")


def cmd_destroy():
    """Stop and destroy the VM."""
    ssh(f"qm stop {VM_ID} 2>/dev/null; sleep 2; qm destroy {VM_ID}")
    print(f"VM {VM_ID} destroyed.")


def cmd_logs():
    """Fetch serial log from Proxmox."""
    result = ssh(f"cat {REMOTE_SERIAL_LOG} 2>/dev/null | tail -50", capture=True)
    if result:
        print(result)
    else:
        print("No serial log available yet.")


def cmd_check_dhcp():
    """Check if DHCP worked by scanning serial log."""
    result = ssh(
        f"grep -E 'DHCP.*Conf|NET.*IP.*10\\.|gateway|ping|assigned|RX delivered' {REMOTE_SERIAL_LOG} 2>/dev/null",
        capture=True
    )
    if result:
        print("=== NETWORK STATUS ===")
        print(result)
    else:
        print("No DHCP activity found in serial log yet.")

    # Also check driver status
    drv = ssh(
        f"grep -c 'IRQ #' {REMOTE_SERIAL_LOG} 2>/dev/null",
        capture=True
    )
    print(f"IRQ count: {drv}")


def main():
    parser = argparse.ArgumentParser(description="Folkering OS Proxmox Deployer")
    parser.add_argument("command", choices=[
        "deploy", "serial", "status", "stop", "destroy", "logs", "check-dhcp"
    ], help="Command to execute")

    args = parser.parse_args()

    commands = {
        "deploy": cmd_deploy,
        "serial": cmd_serial,
        "status": cmd_status,
        "stop": cmd_stop,
        "destroy": cmd_destroy,
        "logs": cmd_logs,
        "check-dhcp": cmd_check_dhcp,
    }

    return commands[args.command]()


if __name__ == "__main__":
    sys.exit(main() or 0)
