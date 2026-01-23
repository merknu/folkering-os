# Build Guide - Folkering OS

**Updated**: 2026-01-23 (Post-restructure)
**Current Status**: Phase 2 Complete (User-mode working)

## Quick Start

### Prerequisites

**Windows (WSL)**:
- Rust nightly toolchain
- QEMU for testing
- WSL environment

**Install Rust** (if not already):
```bash
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
rustup default nightly
```

**Install QEMU**:
```bash
sudo apt update
sudo apt install qemu-system-x86
```

## Build Process

### 1. Build Kernel

```bash
cd ~/folkering/folkering-os/kernel
cargo build --target x86_64-folkering.json --release
```

**Expected Output**:
```
   Compiling folkering-kernel v0.1.0
   ...
   Finished `release` profile [optimized] target(s) in X.XXs
```

**Binary Location**: `kernel/target/x86_64-folkering/release/kernel`
**Binary Size**: ~61 KB

### 2. Create Boot Image

```bash
cd ~/folkering/folkering-os
./tools/create-boot-img.sh
```

**What this does**:
- Copies kernel to boot directory
- Configures Limine bootloader
- Creates bootable .img file

**Output**: `boot/boot.img`

### 3. Test in QEMU

```bash
cd ~/folkering/folkering-os
./tools/test-boot.sh
```

**Or manually**:
```bash
qemu-system-x86_64 \
  -drive file=boot/boot.img,format=raw,if=ide \
  -serial stdio \
  -m 512M
```

**Expected Output**:
```
[Folkering OS] Kernel booted successfully!
[BOOT] Boot information:
[BOOT] Bootloader: Limine 8.7.0
...
[PMM] Total: 510 MB, Usable: 510 MB
[PAGING] Page table mapper ready
[HEAP] Kernel heap ready (16 MB allocated)
[USERMODE] Jumping to userspace...
[SYSCALL] yield called from userspace! ✅
[SYSCALL] yield called from userspace! ✅
...
```

## Repository Structure

```
folkering-os/
├── kernel/              # Build from here
│   ├── src/
│   ├── Cargo.toml
│   └── target/         # Build artifacts
├── boot/               # Boot images output here
│   ├── boot.img
│   └── limine/
├── tools/              # Build scripts
│   ├── create-boot-img.sh
│   └── test-boot.sh
└── docs/               # You are here
```

## Common Tasks

### Clean Build
```bash
cd ~/folkering/folkering-os/kernel
cargo clean
cargo build --target x86_64-folkering.json --release
```

### Check Code
```bash
cd ~/folkering/folkering-os/kernel
cargo check --target x86_64-folkering.json
```

### Run Clippy (Linter)
```bash
cd ~/folkering/folkering-os/kernel
cargo clippy --target x86_64-folkering.json
```

### Format Code
```bash
cd ~/folkering/folkering-os/kernel
cargo fmt
```

## Directory Locations

### Windows
- **Repository**: `C:\Users\merkn\folkering\folkering-os\`
- **Kernel**: `C:\Users\merkn\folkering\folkering-os\kernel\`

### WSL
- **Repository**: `~/folkering/folkering-os/`
- **Kernel**: `~/folkering/folkering-os/kernel/`

### Obsidian Vault (Documentation)
- **Location**: `C:\Users\merkn\OneDrive\Dokumenter\Meray_vault\Meray\Projects\Folkering-OS\`
- **Main Context**: `_claude_context.md`

## Troubleshooting

### Build Fails
1. Check Rust toolchain: `rustup show`
2. Ensure nightly: `rustup default nightly`
3. Clean build: `cargo clean && cargo build --target x86_64-folkering.json --release`

### QEMU Won't Start
1. Check QEMU installed: `qemu-system-x86_64 --version`
2. Install if missing: `sudo apt install qemu-system-x86`
3. Check boot image exists: `ls -lh boot/boot.img`

### Kernel Crashes
1. Check serial output in QEMU (runs with `-serial stdio`)
2. Look for panic messages
3. Common issues:
   - Page fault → Memory mapping issue
   - General Protection Fault → Privilege violation
   - Triple fault → Kernel panic, system reset

## Build Artifacts

### Included in Git
- Source code (`kernel/src/`)
- Build configuration (`Cargo.toml`, `build.rs`, `x86_64-folkering.json`)
- Bootloader (`boot/limine/`)

### Excluded from Git (.gitignore)
- Build artifacts (`kernel/target/`)
- Boot images (`*.img`, `*.iso`)
- Binaries (`*.bin`, `*.elf`)
- Editor files (`.vscode/`, `.idea/`)

## Next Steps After Building

1. **Verify Output**: Check QEMU shows expected messages
2. **Modify Code**: Make changes in `kernel/src/`
3. **Rebuild**: Run build process again
4. **Test**: Boot in QEMU and verify changes

## Current Phase Status

**Phase 2: COMPLETE ✅**
- Memory management (PMM, Paging, Heap)
- GDT/TSS (Privilege separation)
- SYSCALL/SYSRET (Fast system calls)
- User-mode execution (Ring 3)

**Next Phase (Phase 3)**:
- IPC message passing
- Task management
- Process spawning
- Scheduler

## References

- **Technical Context**: See `_claude_context.md` in Obsidian vault
- **Repository Structure**: See root `README.md`
- **Restructure Documentation**: See `REPOSITORY-RESTRUCTURE.md` in Obsidian

---

**Version**: 1.0
**Last Updated**: 2026-01-23
**Phase**: 2 (Complete)
