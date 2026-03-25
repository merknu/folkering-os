# Milestone: Platform Stability - Interrupt-Safe Boot & Ramdisk Filesystem

**Date**: 27. januar 2026
**Status**: COMPLETE

---

## Summary

This is a **Platform Stability Milestone**. We didn't just build a feature; we fixed a deep-seated concurrency bug in the serial driver that would have plagued every future feature (Deadlock on Interrupts), and we replaced the hardcoded `include_bytes!` kernel embedding with a proper boot module filesystem.

The kernel now has a stable, interrupt-safe boot process that loads userspace binaries from a custom Folk-Pack (FPK) filesystem image via Limine boot modules. This is the "Green Light" to move to advanced features.

---

## What Was Built

### 1. Folk-Pack Tool (`tools/folk-pack/`)
Host-side Rust CLI that packs ELF binaries into `initrd.fpk`.

```
+-------------------------------------+
| Header (64 bytes)                   |
|   magic: "FOLK", version: 1         |
|   entry_count, total_size           |
+-------------------------------------+
| Entry Table (64 bytes x N)          |
|   id, type, name[32], offset, size  |
+-------------------------------------+
| Data section (page-aligned blobs)   |
+-------------------------------------+
```

### 2. Ramdisk Driver (`kernel/src/fs/ramdisk.rs`)
Zero-copy parser for FPK images in memory. Finds entries by name, returns raw byte slices.

### 3. Limine Boot Module Integration
`ModuleRequest` loads `initrd.fpk` alongside the kernel. No filesystem needed at boot time.

### 4. Build Pipeline (`tools/docker-test-v2.ps1`)
Automated: build userspace -> pack into FPK -> create boot image -> test in QEMU.

---

## Critical Bugs Fixed

### Bug 1: `serial_println!` Nested `format_args!` Hang
The macro was defined as:
```rust
// BROKEN - nested format_args! hangs
serial_print!("{}\n", format_args!($($arg)*))
```
This produced `_print(format_args!("{}\n", format_args!(...)))` which caused Rust's formatting machinery to hang in this no_std environment.

**Fix**: Rewrote macro to use `concat!`:
```rust
($fmt:expr, $($arg:tt)*) => ($crate::drivers::serial::_print(
    format_args!(concat!($fmt, "\n"), $($arg)*)
));
```

### Bug 2: `write_fmt` / Rust Formatting Pipeline Broken
The entire `core::fmt::Write::write_fmt` code path hangs in this kernel's custom target (`x86_64-folkering`). This affects `serial_println!` with format arguments, `PartialEq` for `&str` and `&[u8]` (which dispatch through trait vtables), and any code path using Rust's formatting machinery.

**Fix**: All init-path serial output converted to bypass macros (`serial_str!`, `serial_strln!`, `write_hex`, `write_dec`) that call `serial.send(byte)` directly without going through `write_fmt`.

### Bug 3: Serial Driver Deadlock with Timer Interrupts
Timer interrupt handler (APIC vector 32) uses `SERIAL1.lock()` for periodic tick logging. If main code holds `SERIAL1` lock when timer fires, deadlock.

**Fix**: All serial functions wrapped with `x86_64::instructions::interrupts::without_interrupts()`.

### Bug 4: `PartialEq` for Slices/Strings Hangs
`e.name[..len] == *name_bytes` and `entry.name_str() == "shell"` both hang due to trait vtable dispatch (same root cause as Bug 2).

**Fix**: Manual byte-by-byte comparison loops:
```rust
let mut matched = true;
for j in 0..len {
    if e.name[j] != name_bytes[j] { matched = false; break; }
}
```

---

## Boot Output (Verified)

```
[RAMDISK] Parsing Folk-Pack initrd...
[RAMDISK] Address: 0xffff80001ff73000, size: 16232 bytes
[RAMDISK] Found Folk-Pack image: 1 entries
[RAMDISK] Entry 0: "shell" (ELF, 12136 bytes)
[BOOT] Loading shell from ramdisk...
[BOOT] Rust shell ELF size: 12136 bytes
[SPAWN_ELF] Starting ELF spawn, binary size=12136
[SPAWN_ELF] ELF parsed, entry=0x202191
[SPAWN_ELF] Task 5 spawn complete!
[BOOT] Task 5 (Rust shell) spawned, id=5
[BOOT] All tasks spawned, starting scheduler...
[SCHED] Scheduler started, entering task execution loop
[SWITCH] switch_to(target_id=1)
```

---

## Architecture After This Milestone

```
Host Build Pipeline:
  cargo build (userspace) -> folk-pack create initrd.fpk -> boot.img

Boot Sequence:
  BIOS -> Limine -> kernel.elf + initrd.fpk (module)
                         |
                    kernel_main()
                         |
              +----------+----------+
              |                     |
         PMM/GDT/IDT          ModuleRequest
         SYSCALL/APIC          -> initrd addr
         Heap/Scheduler        -> Ramdisk::from_memory()
              |                     |
              +----------+----------+
                         |
                  ramdisk.find("shell")
                  task::spawn(elf_data)
                         |
                    Scheduler loop
                    -> switch_to(task)
                    -> IRETQ to Ring 3
```

---

## The Path Forward: From "Blob Store" to "SQL Store"

We currently have a **Flat Filesystem** (Folk-Pack v1). It works perfectly for booting. The vision is to replace this flat list with SQLite.

Before we swap the file format, we should expose the current filesystem to the Shell. Currently, the kernel uses the Ramdisk to boot, but the Shell cannot "see" it.

### Step 1: The `ls` Command (Connect Shell to FS)
Prove the architecture works by letting the Shell list files via a Syscall.

- **Kernel**: Add Syscall 13: `FS_READ_DIR` - reads Ramdisk entries, copies names to userspace
- **LibFolk**: Add `sys_read_dir` wrapper
- **Shell**: Add `ls` command
- **Result**: Type `ls` and see `shell` (and any other packed files)

### Step 2: The SQLite Swap (Backend Upgrade)
Once `ls` works, replace the folk-pack backend with a SQLite parser, keeping the `FS_READ_DIR` syscall interface the same.

- **Host Tool**: Update folk-pack to create a `.sqlite` db (`CREATE TABLE files (id INT, name TEXT, data BLOB)`)
- **Kernel Driver**: Replace `ramdisk.rs` with `sqlite.rs` - parse SQLite B-Tree pages (read-only)
- Note: Writing a full SQL engine in `no_std` is hard. Writing a page traversal to read data is surprisingly manageable.

---

## Files Changed

| File | Change |
|------|--------|
| `kernel/src/drivers/serial.rs` | Interrupt-safe wrappers, bypass functions |
| `kernel/src/lib.rs` | Fixed `serial_println!` macro, ramdisk boot integration |
| `kernel/src/fs/ramdisk.rs` | Manual byte comparison in `find()`, cleanup |
| `kernel/src/arch/x86_64/syscall.rs` | Bypass macros in init path |
| `kernel/src/arch/x86_64/usermode.rs` | Bypass macros in code/stack mapping |
| `kernel/src/arch/x86_64/cpu_freq.rs` | Bypass macros in init |
| `kernel/src/memory/heap.rs` | Bypass macros in init |
| `kernel/src/memory/paging.rs` | Bypass macros in init |
| `kernel/src/task/spawn.rs` | Bypass macros in spawn path |
| `kernel/src/task/switch.rs` | Bypass macros in context switch |
| `kernel/src/task/task.rs` | Bypass macros in Task::new() |
| `kernel/src/task/scheduler.rs` | Bypass macros in scheduler |
| `tools/docker-test-v2.ps1` | Folk-pack build + initrd integration |

**Commit**: `c88855e` on `ai-native-os`
