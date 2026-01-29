# Folkering OS - Current Status

**Date**: 2026-01-29
**Status**: Phase 5 -- Native Semantic Search with Vector Embeddings (IN PROGRESS)
**Repository**: https://github.com/merknu/folkering-os (branch: ai-native-os)

---

## What Works Today

### Kernel (x86_64, ~69KB release)

- Boots via Limine v8.7.0 (UEFI + BIOS)
- Higher-half kernel at 0xFFFFFFFF80000000
- 510 MB RAM detected, PMM + buddy allocator + 16 MB kernel heap
- GDT/TSS with Ring 0 / Ring 3 privilege separation
- SYSCALL/SYSRET fast system calls (FMASK = 0x600)
- Full context switching with CR3 page table switch
- Round-robin cooperative scheduler
- ELF loader (loads binaries from ramdisk)
- Folk-Pack initrd system (custom .fpk format)
- Keyboard IRQ handler (PIC, PS/2 scancodes)
- Serial console (COM1, interrupt-safe)
- 12 active syscalls (see table below)

### Userspace

- **LibFolk SDK**: `#![no_std]` Rust library with `entry!()`, `print!()`/`println!()`, syscall wrappers
- **Shell**: Interactive command-line shell (9 commands)
- **Target**: `x86_64-folkering-userspace` (custom linker script, code at 0x400000)

### Shell Commands (all verified working)

```
folk> help      -- Show available commands
folk> echo hi   -- Echo text back
folk> ls        -- List files in ramdisk (shows ELF/DATA, sizes, names)
folk> cat file  -- Display file contents from ramdisk
folk> ps        -- List running tasks
folk> uptime    -- Show system uptime
folk> pid       -- Show current process ID
folk> clear     -- Clear screen (ANSI escape)
folk> exit      -- Exit shell
```

### Verified Test Output

```
Folkering Shell v0.1.0 (PID: 5)
Type 'help' for available commands.

folk> ls
  ELF     13848 shell
  DATA       57 hello.txt
2 file(s)

folk> cat hello.txt
Hello from Folkering OS!
This file lives in the ramdisk.
```

---

## Syscall Table

| Nr | Name | Description |
|----|------|-------------|
| 7 | YIELD | Yield CPU to scheduler |
| 8 | SERIAL_WRITE | Write to serial console |
| 9 | READ_KEY | Read from keyboard buffer |
| 10 | GET_PID | Get current task ID |
| 11 | EXIT | Exit current task |
| 12 | TASK_LIST | List tasks, return count |
| 13 | FS_READ_DIR | Read ramdisk directory |
| 14 | FS_READ_FILE | Read file from ramdisk |
| 15 | UPTIME | Get uptime in ms |
| 0-2 | IPC | ✅ Working (635 round-trips/15s) |

---

## Known Issues (All Resolved ✅)

| Issue | Fix Commit | Description |
|-------|------------|-------------|
| ~~IPC GPF~~ | `a84b1ae` | Removed align(16) from IpcMessage |
| ~~Uptime 0ms~~ | `aa24ec1` | Fixed tick function, 10ms interval |
| ~~Shared page tables~~ | `1f0aa3d` | Per-task page tables implemented |
| ~~FS_READ_DIR GPF~~ | `b57295b` | Resolved syscall GPF |

**No critical open issues.**

---

## Build & Deploy

```powershell
# Build userspace
cd userspace && cargo build --release

# Pack initrd
cd tools/folk-pack && cargo run -- create ../../boot/iso_root/boot/initrd.fpk ^
  --add shell:elf:../../userspace/target/x86_64-folkering-userspace/release/shell ^
  --add hello.txt:data:../../boot/hello.txt

# Build kernel
cd kernel && cargo build --release

# Deploy to boot image
powershell.exe -File update-boot.ps1
```

---

## Recent Commits (ai-native-os branch)

```
2336668 feat: Phase 5 - Native Semantic Search with Vector Embeddings
14142ee feat: SQLite-backed Filesystem (Universal Container milestone)
a83e8f5 feat: Zero-Copy Shared Memory Pipeline
b57295b fix: resolve GPF in FS_READ_DIR syscall
a84b1ae fix: Remove align(16) from IpcMessage to fix MOVAPS GPF
aa24ec1 fix: Uptime timer now works
1f0aa3d Implement per-task page tables for address space isolation
f60f9fa Implement capability system for IPC security
409ea02 Milestone: IPC message passing working - 635 round-trips in 15 seconds
```

---

## Next Steps

**Completed:**
1. ~~Fix IPC alignment bug~~ ✅
2. ~~Fix uptime timer~~ ✅
3. ~~Per-task page tables~~ ✅
4. ~~SQLite filesystem~~ ✅
5. ~~Zero-copy shared memory~~ ✅

**In Progress (Phase 5):**
- Native semantic search with vector embeddings
- Hybrid keyword + semantic queries

**Future:**
- Desktop environment (Phase 4 - deferred)
- AI-powered features

---

## Phase Summary

| Phase | Status | Date |
|-------|--------|------|
| Phase 1: Boot + Memory | COMPLETE | 2026-01-22 |
| Phase 2: User Mode + Syscalls | COMPLETE | 2026-01-23 |
| Phase 3: ELF + Shell + FS | COMPLETE | 2026-01-28 |
| Phase 3.5: SQLite Universal Container | COMPLETE | 2026-01-29 |
| Phase 4: Desktop | DEFERRED | -- |
| Phase 5: AI / Semantic Search | IN PROGRESS | 2026-01-29 |

---

**Folkering OS** - Norwegian Microkernel OS


---

## UPDATE: 2026-01-29 - Universal Container Milestone

### SQLite-Backed Filesystem: COMPLETE ✅

The OS now serves **structured knowledge** instead of just running code. SQLite is the universal data format.

### New Capabilities

- **libsqlite**: ~950 lines no_std SQLite B-tree reader in userspace
- **folk-pack create-sqlite**: Generate standard SQLite databases
- **Synapse SQLite backend**: Auto-detects files.db, B-tree queries
- **Shell sql command**: `sql "SELECT name FROM files"`

### Boot Output

```
[RAMDISK] Entry 2: "files.db" (DATA, 69632 bytes)
[SYNAPSE] SQLite backend initialized
[SYNAPSE] Ready - database: files.db (3 files)

folk> sql "SELECT name, size FROM files"
synapse          29952
shell            30232
hello.txt           25
```

### Latest Commits

```
2336668 feat: Phase 5 - Native Semantic Search with Vector Embeddings
14142ee feat: SQLite-backed Filesystem (Universal Container milestone)
a83e8f5 feat: Zero-Copy Shared Memory Pipeline
a84b1ae fix: Remove align(16) from IpcMessage to fix MOVAPS GPF
aa24ec1 fix: Uptime timer now works
1f0aa3d Implement per-task page tables for address space isolation
f60f9fa Implement capability system for IPC security
409ea02 Milestone: IPC message passing working - 635 round-trips in 15 seconds
```

### Additional Features (since Phase 3)

- **Per-task page tables**: Full process isolation
- **Capability system**: IPC security model
- **Zero-copy shared memory**: High-performance data transfer
- **Vector embeddings**: Native semantic search support

### Updated Phase Summary

| Phase | Status | Date |
|-------|--------|------|
| Phase 1: Boot + Memory | COMPLETE | 2026-01-22 |
| Phase 2: User Mode + Syscalls | COMPLETE | 2026-01-23 |
| Phase 3: ELF + Shell + FS | COMPLETE | 2026-01-28 |
| Phase 3.5: SQLite Universal Container | COMPLETE | 2026-01-29 |
| Phase 4: Desktop | DEFERRED | -- |
| Phase 5: AI Features / Semantic Search | IN PROGRESS | 2026-01-29 |

---

**Last Updated**: 2026-01-29
