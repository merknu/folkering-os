# Testing Guide - Folkering OS

## Quick Start - Native Windows QEMU (Recommended)

The bootable image `working-boot.img` is ready to test. Docker/Windows output capture has issues, so use native QEMU:

### 1. Install QEMU for Windows

Download from: https://qemu.weilnetz.de/w64/

Install to `C:\Program Files\qemu\` (or your preferred location)

### 2. Run Boot Test

Open PowerShell or CMD in the project directory:

```powershell
# PowerShell
& "C:\Program Files\qemu\qemu-system-x86_64.exe" `
  -drive file=working-boot.img,format=raw,if=ide `
  -serial file:boot-output.log `
  -m 512M `
  -display none `
  -no-reboot

# View output
type boot-output.log
```

```cmd
:: CMD
"C:\Program Files\qemu\qemu-system-x86_64.exe" ^
  -drive file=working-boot.img,format=raw,if=ide ^
  -serial file:boot-output.log ^
  -m 512M ^
  -display none ^
  -no-reboot

:: View output
type boot-output.log
```

### 3. Expected Output

```
[Folkering OS] Kernel booted successfully!
[Folkering OS] Setting up IDT...
[Folkering OS] IDT loaded

==============================================
   Folkering OS v0.1.0 - Microkernel
==============================================

[BOOT] Bootloader: Limine 8.7.0
[PMM] Initializing physical memory manager...
[PAGING] Initializing page table mapper...
[HEAP] Kernel heap ready

[TASK] Creating kernel task (PID 1)...
[TASK] Creating IPC sender task (PID 2)...
[TASK] Creating IPC receiver task (PID 3)...

[SCHED] Starting scheduler...
[SYSCALL] ipc_send_simple(target=3, payload0=0x1234)
[SYSCALL] ipc_receive_simple(from=0)
[SYSCALL] ipc_reply_simple(payload0=0x5678)
[SYSCALL] ipc_send SUCCESS - reply: 0x5678

[TEST] IPC test PASSED!
```

## Alternative: WSL2

If you have WSL2 installed:

```bash
# Install QEMU in WSL2
sudo apt update
sudo apt install qemu-system-x86

# Navigate to project (adjust path for your setup)
cd /mnt/c/Users/merkn/folkering/folkering-os

# Run boot test
qemu-system-x86_64 \
  -drive file=working-boot.img,format=raw,if=ide \
  -serial stdio \
  -m 512M \
  -nographic \
  -no-reboot

# Output appears directly in terminal
```

## Alternative: VirtualBox

Convert image and boot in VirtualBox:

```bash
# Convert to VDI
VBoxManage convertfromraw working-boot.img working-boot.vdi

# Create VM
VBoxManage createvm --name "Folkering-Test" --register
VBoxManage modifyvm "Folkering-Test" --memory 512 --vram 16

# Attach disk
VBoxManage storagectl "Folkering-Test" --name "SATA" --add sata
VBoxManage storageattach "Folkering-Test" --storagectl "SATA" \
  --port 0 --device 0 --type hdd --medium working-boot.vdi

# Configure serial output
VBoxManage modifyvm "Folkering-Test" --uart1 0x3F8 4
VBoxManage modifyvm "Folkering-Test" --uartmode1 file boot-output.log

# Start VM
VBoxManage startvm "Folkering-Test" --type headless

# View output
cat boot-output.log
```

## Alternative: Physical Hardware

**WARNING: This will erase the target drive!**

Write image to USB and boot on real hardware:

```bash
# Linux/WSL2 (replace /dev/sdX with your USB device)
sudo dd if=working-boot.img of=/dev/sdX bs=4M status=progress
sync

# Windows (use Rufus or similar tool)
# Select working-boot.img as ISO/Disk image
# Select target USB drive
# Write in DD mode
```

## Troubleshooting

### No output in boot-output.log

**Symptom**: File is empty or very small

**Solutions**:
1. Check QEMU path is correct
2. Ensure `working-boot.img` exists in current directory
3. Try with `-nographic` instead of `-display none`
4. On WSL2, use `-serial stdio` to see output directly

### QEMU doesn't start

**Symptom**: Error about missing files or invalid disk

**Solutions**:
1. Verify boot image exists: `ls -lh working-boot.img`
2. Check image integrity: `file working-boot.img`
   - Should show: "DOS/MBR boot sector"
3. Rebuild image if needed: `bash tools/working-boot.sh`

### Kernel panics or crashes

**Symptom**: Output shows panic messages or exception dumps

**Solutions**:
1. Check debug output for specific error
2. Rebuild kernel: `cd kernel && cargo build --release`
3. Recreate boot image with new kernel
4. Report issue with full error output

## Build from Scratch

If you need to rebuild everything:

```bash
# Windows (Git Bash)
bash tools/working-boot.sh

# This will:
# 1. Build kernel (release mode)
# 2. Create 50MB disk with MBR partition
# 3. Format FAT32 filesystem
# 4. Install Limine bootloader
# 5. Copy kernel and config files
# 6. Verify structure

# Output: working-boot.img (50 MB)
```

## Files

- **working-boot.img** (50 MB) - Main bootable disk image
- **kernel/target/x86_64-folkering/release/kernel** (69 KB) - Compiled kernel
- **boot/limine.conf** (428 bytes) - Bootloader configuration
- **boot/limine/bin/** - Limine bootloader binaries

## Testing Option B IPC

The kernel is configured to test Option B (register-based IPC):

1. **Task 1 (Kernel)**: Spawns other tasks
2. **Task 2 (Sender)**: Sends IPC message with payload 0x1234 to Task 3
3. **Task 3 (Receiver)**: Receives message, replies with 0x5678
4. **Task 2**: Receives reply, verifies payload matches

Look for these messages in output:
```
[SYSCALL] ipc_send_simple(target=3, payload0=0x1234, payload1=0x0)
[SYSCALL] ipc_receive_simple(from=0)
[SYSCALL] ipc_reply_simple(payload0=0x5678, payload1=0x0)
[SYSCALL] ipc_send SUCCESS - reply payload: 0x5678
```

If you see "IPC test PASSED!" - Option B is working!

## Next Steps After Testing

Once Option B boots and shows IPC working:

1. **Measure Performance**: Add cycle counting to measure:
   - Context switch time (target: <500 cycles)
   - IPC round-trip latency (target: <1000 cycles)

2. **Implement Option A**: If Option B performance is acceptable, implement stack-based IPC for comparison

3. **Phase 4**: Move to memory management (paging, heap expansion, CoW)

## Need Help?

See `BOOT-TEST-STATUS.md` for detailed status and troubleshooting information.
