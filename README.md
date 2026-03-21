# Folkering OS

**An AI-native operating system written from scratch in Rust.** Built on a microkernel architecture where AI inference is a first-class kernel service, not an afterthought.

Folkering OS runs on bare-metal x86-64 hardware (via QEMU) with its own kernel, compositor, shell, filesystem, IPC system, and a complete on-device SLM inference engine. No libc, no POSIX, no Linux — pure `no_std` Rust from bootloader to token generation.

## What It Can Do Today

```
> ask hi
[AI] Thinking...
  <SmolLM2-135M generates 64 tokens in real-time>

> ai-status
AI: model loaded
Arena: 8MB

> ls
  synapse      51672
  shell        48824
  compositor  109944
  inference    64944
  files.db    253952
  hello.txt       25

> open calc
  → [Calculator] WASM-powered app with button grid
```

## Architecture

Six userspace services communicate via synchronous IPC over shared memory:

```
                    +------------------+
                    |    Compositor    |  GPU framebuffer, window manager,
                    |   (Task 4)      |  omnibar, terminal, mouse/keyboard
                    +--------+---------+
                             |
              +--------------+--------------+
              |                             |
     +--------+---------+         +--------+---------+
     |  Intent Service  |         | Inference Server |
     |   (Task 5)       |         |   (Task 6)       |
     |  Command routing |         |  SLM, tokenizer  |
     +--------+---------+         +------------------+
              |                             |
     +--------+---------+         +--------+---------+
     |      Shell       |         |     Synapse      |
     |   (Task 3)       |         |   (Task 2)       |
     |  Commands, WASM  |         |  SQLite, VFS     |
     +------------------+         +------------------+
              |                             |
     +--------+-----------------------------+---------+
     |              Folkering Kernel                  |
     |  SYSCALL/SYSRET, preemptive scheduler, APIC   |
     |  VirtIO DMA, shmem, mmap, IPC, page tables    |
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

### On-Device AI Inference

The inference engine runs as Task 6 — a dedicated userspace service with its own 8MB arena, KV-cache, and BPE tokenizer. The full pipeline:

```
User types "ask hi" in Omnibar
     │
     ▼
Compositor creates shmem with prompt text
     │  IPC send (shmem handle + length)
     ▼
Inference Server (Task 6):
  1. BPE tokenize (Greedy Prefix Match, ▁-aware)
  2. Prepend BOS token
  3. Prefill: N tokens × 30 layers forward pass
  4. Generate: autoregressive loop (max 64 tokens)
     - Q4_0/Q8_0 GEMM, RMSNorm, RoPE, SiLU-gated FFN
     - KV-cache with sink-token eviction
     - Top-P nucleus sampling + repetition penalty
     - Logit clamping (ULTRA 31)
  5. Write response to shmem, reply via IPC
     │
     ▼
Compositor displays generated text in terminal window
```

Model: SmolLM2-135M-Instruct (Q4_0, 87MB) loaded from VirtIO disk via DMA bursting.

### Semantic VFS

Files live in a SQLite database on a VirtIO block device. Synapse (the "data kernel") provides file listing, content reading, and text search — all via IPC:

```
Compositor → Intent Service → Shell → Synapse → SQLite (VirtIO)
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
| **Preemptive multitasking** | Done | Timer IRQ, priority scheduler, deadline support |
| **SYSCALL/SYSRET** | Done | SWAPGS + CpuLocal per-CPU storage |
| **FXSAVE/FXRSTOR** | Done | XMM/SSE state preserved across switches |
| **Page tables** | Done | Per-task PML4, HHDM, user/kernel separation |
| **Shared memory** | Done | Create, map, grant, unmap, destroy — correct page table |
| **SYS_MMAP/MUNMAP** | Done | Anonymous pages, chunked allocation (16MB/call) |
| **IPC** | Done | Synchronous send/reply, async recv, CallerToken |
| **VirtIO block** | Done | DMA burst reads (64 sectors/request), FOLKDISK header |
| **VirtIO network** | Done | DHCP, ICMP ping, DNS resolution |
| **I/O APIC** | Done | Keyboard + mouse + VirtIO interrupt routing |
| **Panic screen** | Done | Graphical panic with register dump, recursion guard |

## Userspace

| Component | Purpose | Lines |
|-----------|---------|-------|
| **Compositor** | Window manager, terminal, omnibar, mouse/keyboard | ~3000 |
| **Inference Server** | GGUF model loading, tokenizer, transformer, sampling | ~1000 |
| **Shell** | Command execution, WASM apps, IPC handlers | ~2400 |
| **Synapse** | SQLite parser, file cache, VirtIO persistence | ~1100 |
| **Intent Service** | Capability-based command routing | ~200 |
| **libtensor** | Q4_0/Q8_0 GEMM, RMSNorm, RoPE, KV-cache, GGUF, BPE | ~2200 |
| **libfolk** | Syscall wrappers, IPC, shmem, inference, UI protocol | ~1000 |
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
- WSL2 with Ubuntu (for cross-compilation and mtools)
- QEMU x86_64
- Python 3.12 (for MCP servers)

### Quick Start

```bash
# Build kernel + userspace + pack initrd + run QEMU
# (via MCP server tools — see folkering-mcp/server.py)
folkering_rebuild_run()

# Or manually:
cd kernel && cargo build --release
cd userspace && cargo build --release
cd tools/folk-pack && cargo run --release -- create boot/initrd.fpk ...
qemu-system-x86_64 -drive file=boot/current.img -serial file:serial.log -m 512M
```

### AI Model Setup

```bash
# Download SmolLM2-135M-Instruct Q4_0 (~87MB)
curl -L -o boot/model.gguf \
  "https://huggingface.co/amai-gsu/SmolLM2-135M-Instruct-Q4_0-GGUF/resolve/main/smollm2-135m-instruct-q4_0.gguf"

# Pack model into VirtIO disk (ULTRA 26: 4KB-aligned)
cd tools/folk-pack && cargo run --release -- pack-model boot/virtio-data.img boot/model.gguf
```

Model loads in ~45 seconds via multi-sector DMA bursting (ULTRA 36).

### MCP Servers

**folkering-os** (build/run/interact):
- `folkering_rebuild_run` — full build→pack→boot cycle
- `folkering_screenshot` — GUI capture via QMP
- `folkering_interact` — scripted keyboard/mouse sequences

**folkering-debug** (live debugging):
- `kernel_symbol_lookup` — resolve hex addresses to function names
- `serial_throttle_analyzer` — collapse loop patterns in serial logs
- `qemu_inspect_registers` — live CPU state via QMP

## Roadmap

### Epoch 1: Cognitive Infrastructure (Current)

- [x] Phase 1-2: Kernel boot, PMM, paging, syscalls, user mode
- [x] Phase 3-5: IPC, shmem, capability system
- [x] Phase 6-8: Compositor, window manager, Neural Desktop
- [x] Milestone 2.3: IPC+shmem architecture (page table fix)
- [x] Milestone 3: Synapse SQLite integration, semantic VFS
- [x] Milestone 4: Interactive terminal, SYS_MMAP/MUNMAP, App Weaver
- [x] M26-M30: VirtIO network, DHCP, DNS, ICMP ping, TLS 1.3
- [x] M31-M32: GitHub API, JSON parser, clone-to-VFS
- [x] **M33-M41: libtensor** — Q4_0/Q8_0 GEMM, RMSNorm, RoPE, SiLU, KV-cache, GGUF parser, BPE tokenizer, transformer forward pass
- [x] **M42: First Words** — SmolLM2-135M generates tokens on bare metal

### Next: Epoch 1 Completion

- [ ] Token streaming (IPC per token for typewriter effect)
- [ ] AVX2 GEMM acceleration (requires kernel XSAVE support)
- [ ] Conversation context / multi-turn chat
- [ ] Smaller model support (TinyStories-33M for faster iteration)

### Epoch 2-3: Agent-Native Paradigm (Vision)

1. **MIMO State-Space Scheduling** — Replace priority schedulers with predictive state-space models
2. **Generative Latent Memory** — Memory that reconstructs rather than retrieves
3. **Hyperdimensional I/O** — Semantic device abstraction via high-dimensional vectors
4. **Active Inference Immunity** — Security via free energy minimization, not signatures
5. **Declarative Intent Syscalls** — `SYS_DO_INTENT("summarize this")` — apps describe intent, OS implements

## Author

Knut Ingmar Merødningen

## License

To be determined.
