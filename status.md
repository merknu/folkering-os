# Folkering OS - Status

---
tags: [project, os-development, rust, kernel]
date: 2026-01-22
status: boot-successful-minimal-kernel
---

## Project Overview

**Folkering OS** - A capability-based microkernel operating system written in Rust

- **Architecture**: x86_64 higher-half kernel at 0xFFFFFFFF80000000
- **Boot Protocol**: Limine v0.5
- **Design**: Microkernel with capability-based security
- **IPC**: 64-byte cache-aligned messages (<1000 cycle target)
- **Language**: Pure Rust (no_std)
- **Repository**: https://github.com/merknu/folkering-os (private)

## Current Status (2026-01-22 Latest)

### ✅ Boot Testing: SUCCESSFUL!

**Status**: Kernel boots successfully with visible serial output! 🎉

**Boot Test Results:**
- ✅ ISO created successfully (3.5MB)
- ✅ UEFI boot sequence works perfectly
- ✅ Limine bootloader loads kernel at 0xffffffff80000000
- ✅ Serial output working (COM1, 38400 baud, 8N1)
- ✅ Kernel initializes BSS section
- ✅ Kernel enters stable HLT loop
- ✅ Code committed to GitHub

**Working Boot Sequence:**
```
Limine → Kernel at 0xffffffff80000000 → BSS cleared → Serial init → HLT loop
```

**Serial Output:**
```
Folkering OS kernel starting...
BSS cleared, serial initialized
Basic initialization complete, entering idle loop
```

**QEMU Command (working):**
```bash
qemu-system-x86_64 -cdrom folkering-simple.iso -m 512M \
  -bios /usr/share/ovmf/OVMF.fd -nographic -serial mon:stdio
```

### ✅ Kernel Development: MINIMAL KERNEL COMPLETE

**Current Implementation:**
The kernel has a working minimal implementation with inline initialization:

**Entry Point (src/lib.rs):**
- Pure Rust `_start()` function (no external assembly)
- BSS section clearing (using `&raw mut` syntax)
- Inline COM1 serial port initialization (no external modules)
- Basic debug output to serial console
- Stable HLT loop

**What Works:**
- ✅ Boot via Limine bootloader
- ✅ Higher-half kernel mapping (0xFFFFFFFF80000000)
- ✅ BSS section initialization
- ✅ Serial console output (COM1)
- ✅ CPU halt loop (stable idle state)

**What's Not Yet Integrated:**
- ⚠️ Subsystem modules (drivers, boot, memory, IPC, task, etc.)
- ⚠️ Limine boot info parsing
- ⚠️ Memory management initialization
- ⚠️ Capability system initialization
- ⚠️ IPC subsystem initialization

**Binary Details:**
- Kernel size: ~30KB (optimized release build)
- Entry point: 0xFFFFFFFF80000000
- Serial: COM1 @ 0x3F8 (38400 baud, 8N1)

### ✅ Version Control: COMPLETE

**Repository Setup:**
- ✅ Git repository initialized
- ✅ Initial commit with working kernel (1777 files)
- ✅ .gitignore configured (excludes build artifacts)
- ✅ Build artifacts cleaned from repo
- ✅ Pushed to GitHub: https://github.com/merknu/folkering-os
- ✅ Repository type: Private

**Git History:**
```
commit 2: Add .gitignore and remove build artifacts
commit 1: Initial commit - Working Folkering OS kernel with Limine boot
```

**Files Tracked:**
- Kernel source code (src/)
- Cargo configuration (Cargo.toml, .cargo/config.toml)
- Build configuration (x86_64-folkering.json, linker.ld)
- Bootloader configuration (limine.conf)
- Documentation (architecture docs, README, etc.)

## Technical Details

### Compilation Environment
- **Platform**: Windows 11
- **Rust**: nightly toolchain
- **Target**: x86_64-folkering (custom spec)
- **Build**: `cargo build --release`
- **Features**: `build-std`, `build-std-features = ["compiler-builtins-mem"]`

### Memory Layout
```
0xFFFFFFFF80000000  ← Kernel virtual base (higher-half)
0xFFFF800000000000  ← HHDM (Higher-half direct map)
```

### Build System
- Custom target spec: `x86_64-folkering.json`
- Linker script: `linker.ld`
- Entry point: `_start` in `src/lib.rs`
- Main: `kernel_main` in `src/main.rs`

## Development Strategy: Core-First, Simple & Clean

**Philosophy**: Don't be blinded by adding huge functions in the beginning. Keep it simple and clean.

**Long-term Goal**: Full OS that can compete with Windows, Mac, Ubuntu
**Current Focus**: Build a solid, minimal core first

---

## Next Steps: Minimal & Essential Only

### Phase 1: Core Basics (1-2 weeks) - CURRENT FOCUS

**Goal**: Get fundamental kernel services working - nothing more!

1. **Boot Info Parsing** (2-3 days)
   - Parse Limine boot info structure
   - Extract memory map (usable vs reserved regions)
   - Record initrd location and size
   - ⚠️ Keep it simple - just parse, don't use yet

2. **Basic Memory Management** (3-5 days)
   - Initialize buddy allocator (physical pages)
   - Basic page table setup (virtual memory)
   - Simple heap allocator (for Vec, Box)
   - ⚠️ Minimal implementation only

3. **Stabilize and Test** (2-3 days)
   - Verify memory allocation works
   - Test dynamic data structures (Vec, BTreeMap)
   - Fix any crashes or panics
   - ⚠️ **STOP HERE - Don't proceed until stable**

### Phase 2: Minimal IPC & Tasks (1-2 months)

**Goal**: Get one userspace process running - that's it!

1. **Simple IPC** (1-2 weeks)
   - Synchronous send/receive only
   - No shared memory yet - just messages
   - No optimizations - keep it simple

2. **Basic Task Creation** (1-2 weeks)
   - Spawn single userspace process
   - Simple context switching
   - No scheduler yet - just two tasks

3. **First Userspace Process** (1-2 weeks)
   - Load ELF from initrd
   - Jump to userspace
   - Verify it runs (print to serial)
   - ⚠️ **STOP - Evaluate before adding more**

### Phase 3: Expand Core (3-6 months)

**Only after Phase 1 & 2 are rock-solid:**

- Full IPC with shared memory
- Basic scheduler (simple round-robin first)
- Capability system (security)
- Multiple processes running

### Long-term Vision (6-12+ months)

**AI-Powered OS Development:**
- MCP integration (AI-to-OS communication)
- AI-generated drivers (see `ai-agent-army.md`)
- Automated code generation for components
- Full desktop environment
- Compete with Windows/Mac/Ubuntu

---

## Current Mantra

> **"Simple and clean. Core first. One thing at a time."**

Don't rush to userspace services, VFS, network stack, or fancy features. Get the fundamentals perfect first.

## Files & Documentation

**Location**: `C:\Users\merkn\OneDrive\Dokumenter\Meray_vault\Meray\Projects\Folkering-OS\code\kernel\`

**Key Documents**:
- [[TESTING-GUIDE.md]] - How to boot and test
- [[MCP-INTEGRATION.md]] - AI integration vision
- [[NEXT-STEPS.md]] - Development roadmap

**Source Code**: [[code/kernel/src/]]

## Notes

**Why This Matters:**
- European OS development
- Rust-based systems programming
- Capability-based security model
- Microkernel architecture
- AI integration (MCP) from ground up

**Lessons Learned:**
- AT&T assembly syntax incompatible with Rust's inline asm
- Linker scripts need PROVIDE() for external symbol access
- WSL tooling can be fragile (apt issues)
- Pure Rust entry points are viable for bare-metal

**Performance Targets:**
- Boot time: <10 seconds
- IPC latency: <1000 CPU cycles
- Context switch: <500 cycles
- Scheduling: <10,000 cycles

---

**Last Updated**: 2026-01-22 Evening
**Recent Achievement**: ✅ Kernel boots successfully with serial output!
**Current Focus**: Integrate subsystem modules into boot sequence
**Next Milestone**: Module integration → userspace services → v1.0 release
