# Phase 2 Progress - Folkering OS

**Date**: 2026-01-23
**Status**: 🚧 In Progress - GDT/TSS Complete

---

## ✅ Completed: GDT/TSS Implementation

### What Was Implemented

**File**: `src/arch/x86_64/gdt.rs`

**Features Added**:
1. ✅ **Task State Segment (TSS)**
   - 16 KB syscall stack for Ring 3 → Ring 0 transitions
   - Privilege Stack Table entry (RSP0) configured
   - Proper stack pointer for SYSCALL instruction

2. ✅ **Complete Segment Descriptors**
   - Kernel Code Segment (0x08) - Ring 0
   - Kernel Data Segment (0x10) - Ring 0
   - User Code Segment (0x1B) - Ring 3
   - User Data Segment (0x23) - Ring 3
   - TSS Descriptor (0x28-0x2F) - Takes 2 GDT entries on x86-64

3. ✅ **Helper Functions**
   - `kernel_code_selector()` - For interrupt returns
   - `kernel_data_selector()` - For kernel data access
   - `user_code_selector()` - For SYSRET
   - `user_data_selector()` - For SYSRET

4. ✅ **Initialization Sequence**
   - Load GDT with `lgdt` instruction
   - Set CS register to kernel code segment
   - Set DS register to kernel data segment
   - Load TSS with `ltr` instruction

### Boot Integration

**File**: `src/lib.rs`

Added GDT/TSS initialization after PMM but before paging:

```rust
// Initialize GDT and TSS
serial_println!("[INIT] Initializing GDT and TSS...");
arch::x86_64::gdt_init();
serial_println!("[GDT] Global Descriptor Table and Task State Segment loaded\n");
```

**Initialization Order** (now):
1. Serial output
2. IDT (exception handlers)
3. Boot info parsing
4. PMM (physical memory)
5. **GDT/TSS** ← NEW
6. Paging (virtual memory)
7. Heap (dynamic allocation)
8. Tests

### Build Status

✅ **Kernel builds successfully**:
```
Compiling folkering-kernel v0.1.0
Finished `release` profile [optimized] target(s) in 2.82s
```

Binary size: **36 KB** (release build)

---

## 🧪 How to Test

### 1. Create Boot Image (WSL Ubuntu-22.04)

```bash
cd /mnt/c/Users/merkn/folkering/kernel-src

# Create 100MB disk image
dd if=/dev/zero of=/tmp/boot-v2.img bs=1M count=100

# Create DOS partition table with bootable partition
echo "label: dos
start=2048, type=83, bootable" | sfdisk /tmp/boot-v2.img

# Format partition as FAT32 (offset 1MB)
mformat -i /tmp/boot-v2.img@@1M -F -v BOOT ::

# Copy kernel
mcopy -o -i /tmp/boot-v2.img@@1M target/x86_64-folkering/release/kernel ::/boot/kernel.elf

# Copy limine config
mcopy -i /tmp/boot-v2.img@@1M limine.conf ::

# Copy limine bootloader files
mcopy -i /tmp/boot-v2.img@@1M limine/limine-bios.sys ::/

# Install Limine to MBR
./limine/limine bios-install /tmp/boot-v2.img
```

### 2. Test in QEMU

```bash
qemu-system-x86_64 \
  -drive file=/tmp/boot-v2.img,format=raw,if=ide \
  -serial stdio \
  -m 512M \
  -display none \
  -no-reboot
```

### 3. Expected Output

```
[Folkering OS] Kernel booted successfully!
[Folkering OS] Setting up IDT...
[Folkering OS] IDT loaded

==============================================
   Folkering OS v0.1.0 - Microkernel
==============================================

[BOOT] Boot information:
[BOOT] Bootloader: Limine 8.7.0
...

[PMM] Total: 510 MB, Usable: 510 MB
[PMM] Bootstrap allocator ready

[INIT] Initializing GDT and TSS...
[GDT] Global Descriptor Table and Task State Segment loaded  ← NEW

[INIT] Initializing page table mapper...
[PAGING] Page table mapper ready

[INIT] Initializing kernel heap...
[HEAP] Kernel heap ready (16 MB allocated)

[TEST] Testing dynamic memory allocation...
[TEST]   Vec::push() works: [1, 2, 3]
[TEST]   String::from() works: Folkering OS
[TEST] All allocation tests passed!

[BOOT] ✅ Phase 1 COMPLETE - Memory subsystem functional!
[BOOT] Entering halt loop...
```

### 4. Verification

✅ **Success Criteria**:
- Kernel boots without panics
- GDT/TSS initialization message appears
- Memory tests still pass
- No triple faults or exceptions

---

## 📋 What's Next: SYSCALL Support

### File: `src/arch/x86_64/syscall.rs`

**Already exists** but needs initialization:

1. ✅ Syscall entry assembly (SYSCALL/SYSRET)
2. ✅ Syscall handler with 8 syscall numbers
3. ⏸️ **Not initialized yet** - needs MSR setup

### Next Steps

**File to modify**: `src/lib.rs`

After GDT/TSS init, add:

```rust
// Initialize syscall support (SYSCALL/SYSRET instructions)
serial_println!("[INIT] Initializing SYSCALL/SYSRET support...");
arch::x86_64::syscall_init();
serial_println!("[SYSCALL] Fast system call interface ready\n");
```

This will:
- Enable SYSCALL/SYSRET via EFER MSR
- Set LSTAR MSR to syscall entry point
- Configure STAR MSR for segment selectors
- Enable user → kernel transitions

---

## 🎯 Phase 2 Roadmap Progress

### ✅ Completed
- [x] GDT with kernel/user segments
- [x] TSS with syscall stack
- [x] GDT/TSS initialization
- [x] Boot integration

### 🚧 In Progress
- [ ] SYSCALL/SYSRET initialization
- [ ] Test user mode transition

### ☐ Pending
- [ ] IPC message queue implementation
- [ ] Task structure and task table
- [ ] Process spawning (spawn syscall)
- [ ] Context switching
- [ ] Basic scheduler

---

## 📊 Code Statistics

**Phase 2 Code So Far**:
- `src/arch/x86_64/gdt.rs`: 115 lines (complete)
- `src/arch/x86_64/syscall.rs`: 291 lines (scaffolding, needs init)
- `src/ipc/message.rs`: 170 lines (complete)
- `src/ipc/*.rs`: ~600 lines total (scaffolding)
- `src/task/*.rs`: ~800 lines total (scaffolding)

**Total Phase 2 Code**: ~2,000 lines (mix of complete + scaffolding)

---

## 🔧 Architecture Decisions

### Why TSS?

Modern x86-64 doesn't use segmentation for memory isolation (uses paging instead), but the TSS is **required** for:

1. **Syscall Stack Switching**: When SYSCALL instruction executes, CPU needs to know where the kernel stack is. TSS.RSP0 provides this.

2. **Interrupt Stack Switching**: When interrupt occurs in user mode, CPU switches to kernel stack from IST (Interrupt Stack Table) in TSS.

3. **Hardware Requirement**: x86-64 requires a valid TSS to be loaded even if not using task switching.

### Why Both GDT and TSS?

- **GDT**: Defines code/data segments for kernel (Ring 0) and user (Ring 3)
- **TSS**: Defines stacks to use when transitioning privilege levels
- Both are hardware-required parts of x86-64 protection mechanisms

---

**Next Session**: Initialize SYSCALL support and test user mode transitions

**Status**: Ready for testing! 🚀
