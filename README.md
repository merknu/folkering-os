# Folkering OS

**An AI-native operating system written from scratch in Rust.** Built on a microkernel architecture where AI agents are first-class citizens, not afterthoughts.

Folkering OS runs on bare-metal x86-64 hardware (via QEMU) with its own kernel, compositor, shell, filesystem, and IPC system. Applications describe their UI declaratively and the OS renders it — no GUI toolkit required.

## What It Can Do Today

```
folk> ls
  synapse    38632
  shell      17312
  compositor 73920
  hello.txt  25

folk> cat hello.txt
Hello from Folkering OS!

folk> find shell
Matches: 1
  shell 17312

folk> ps
1 idle     Run
2 synapse  Run
3 shell    Blk
4 compositor Blk
5 intent-service Blk

folk> app
App received from Shell!
  → [Folkering App] window with [OK] [Cancel] buttons
```

## Architecture

Five userspace services communicate via synchronous IPC over shared memory:

```
                    +------------------+
                    |    Compositor    |  GPU framebuffer, window manager,
                    |   (Task 4)      |  widget renderer, mouse/keyboard
                    +--------+---------+
                             |
                    +--------+---------+
                    |  Intent Service  |  Capability-based command routing
                    |   (Task 5)      |
                    +--------+---------+
                             |
              +--------------+--------------+
              |                             |
     +--------+---------+         +--------+---------+
     |      Shell       |         |     Synapse      |
     |   (Task 3)       |         |   (Task 2)       |
     |  Commands, apps  |         |  SQLite, search  |
     +------------------+         +------------------+
              |                             |
     +--------+-----------------------------+---------+
     |              Folkering Kernel                  |
     |  SYSCALL/SYSRET, SWAPGS, FXSAVE, preemption  |
     |  Page tables, shmem, mmap, IPC, scheduler     |
     +------------------------------------------------+
```

### The App Weaver

Shell builds UI declaratively and sends it to Compositor via shared memory:

```rust
// Shell builds a widget tree (no GUI code!)
let mut w = UiWriter::new(&mut buf);
w.header("My App", 280, 160);
w.vstack_begin(6, 3);
  w.label("Hello from Shell!", 0x00CCFF);
  w.hstack_begin(8, 2);
    w.button("OK", 1, 0x226644, 0xFFFFFF);
    w.button("Cancel", 2, 0x664422, 0xFFFFFF);

// Serialize to shmem, send via IPC → Compositor renders it
// Button clicks route back to Shell as action_id events
```

Apps never touch pixels. Compositor renders. Shell weaves.

### Semantic VFS

Files live in a SQLite database inside the ramdisk. Synapse (the "data kernel") provides file listing, content reading, and text search — all via IPC:

```
Compositor → Intent Service → Shell → Synapse → SQLite
     ^                                              |
     └──── shmem results (zero-copy) ──────────────┘
```

### Shared Memory (shmem)

Zero-copy data transfer between tasks. Each task has its own page table — `shmem_map` maps physical pages into the correct PML4:

```rust
let handle = shmem_create(4096)?;      // allocate physical page
shmem_grant(handle, target_task)?;     // grant access
shmem_map(handle, 0x30000000)?;        // map into MY page table
// ... write data ...
shmem_unmap(handle, 0x30000000)?;      // unmap
// Target task maps same handle at their own virtual address
```

## Kernel Features

| Feature | Status | Details |
|---------|--------|---------|
| **Preemptive multitasking** | Done | Timer IRQ, RPL-checked, per-task context |
| **SYSCALL/SYSRET** | Done | SWAPGS + CpuLocal per-CPU storage |
| **FXSAVE/FXRSTOR** | Done | XMM/SSE state preserved across switches |
| **Page tables** | Done | Per-task PML4, HHDM, user/kernel separation |
| **Shared memory** | Done | Create, map, grant, unmap, destroy — correct page table |
| **SYS_MMAP/MUNMAP** | Done | Anonymous pages, zero-filled, freed to PMM |
| **IPC** | Done | Synchronous send/reply, async recv, CallerToken |
| **I/O APIC** | Done | Keyboard + mouse interrupt routing |
| **Panic screen** | Done | Graphical panic with register dump, recursion guard |

## Userspace

| Component | Purpose | Lines |
|-----------|---------|-------|
| **Compositor** | Window manager, widget renderer, omnibar, mouse/keyboard | ~1700 |
| **Shell** | Command execution, app builder, IPC command handlers | ~1300 |
| **Synapse** | SQLite parser, file cache, text search | ~1000 |
| **Intent Service** | Capability-based command routing | ~200 |
| **libfolk** | Syscall wrappers, IPC, shmem, UI wire protocol | ~800 |
| **libsqlite** | Custom no_std SQLite B-tree reader | ~500 |

## UI Wire Protocol

Binary format for declarative UI trees (`libfolk/src/ui.rs`):

```
Header:  [magic:"FKUI"][ver:1][title_len:1][width:2][height:2][title:N]
Widgets: [tag:1][type-specific data][children recursively]

Tags: 0x01=Label, 0x02=Button, 0x03=VStack, 0x04=HStack, 0x05=Spacer
```

Zero-alloc serialization (`UiWriter`) and deserialization (`parse_widget`). No serde, no alloc — pure `no_std`.

## Building & Running

### Prerequisites

- Rust nightly (x86_64-unknown-linux-gnu target)
- WSL2 with Ubuntu (for cross-compilation)
- QEMU x86_64
- Python 3.12 (for MCP debug server)

### Quick Start

```bash
# Build kernel + userspace + pack initrd + run QEMU
# (via MCP server tools — see mcp/server.py)
folkering_rebuild_run()

# Or manually:
cd kernel && cargo build --release
cd userspace && cargo build --release --target x86_64-folkering-userspace
python tools/folk-pack create boot/initrd.fpk ...
qemu-system-x86_64 -drive file=boot/current.img -serial file:serial.log -m 512M
```

### MCP Debug Server

Three tools for live debugging:

- `kernel_symbol_lookup` — resolve hex addresses to function names
- `serial_throttle_analyzer` — collapse loop patterns in serial logs
- `qemu_inspect_registers` — live CPU state via QMP

## Roadmap

### Completed

- [x] Phase 1-2: Kernel boot, PMM, paging, syscalls, user mode
- [x] Phase 3-5: IPC, shmem, capability system
- [x] Phase 6-8: Compositor, window manager, Neural Desktop
- [x] Milestone 2.3: IPC+shmem architecture (page table fix)
- [x] Milestone 3: Synapse SQLite integration, semantic search
- [x] Milestone 4: Interactive terminal, SYS_MMAP/MUNMAP, App Weaver, button clicks

### Next

- [ ] **WASM Runtime**: Integrate wasmi for third-party apps
- [ ] **Persistent Storage**: VirtIO block device driver
- [ ] **Vector Search**: On-device embeddings for semantic file search
- [ ] **SYS_DO_INTENT**: Declarative intent syscalls (Phase 13+ vision)

## Phase 13+ Vision: The Agent-Native Paradigm

Four concepts that define the long-term direction:

1. **Zero-Boundary WASM UI** — AI-generated apps compile to WASM and run inside Compositor as plugins, eliminating IPC overhead for UI rendering.

2. **Clairvoyant Paging** — Synapse pre-fetches pages to RAM before the user requests them, based on predictive patterns.

3. **Immutable Semantic VFS** — The filesystem is an event-sourced log via SQLite. Files are never overwritten — changed via semantic diffs. Users can ask Synapse to "rewind" the filesystem to any point in time.

4. **Declarative Intent Syscalls** — `SYS_DO_INTENT("save this as thumbnail")` — apps describe what they want, the OS figures out how.

## Author

Knut Ingmar Merødningen

## License

To be determined.
