# Folkering OS Architecture

> A bare-metal AI-native operating system. Not a Linux distribution. Written from scratch in Rust `no_std` for x86-64.

## Overview

Folkering OS is designed around a single premise: **AI inference is a first-class kernel service, not an afterthought.** The entire stack — from bootloader to token generation — is custom Rust with zero dependency on libc, POSIX, or any existing OS.

```
┌─────────────────────────────────────────────────────────┐
│                    User Input                           │
│           Keyboard / Mouse / COM3 God Mode              │
└────────────────────┬────────────────────────────────────┘
                     │
┌────────────────────▼────────────────────────────────────┐
│                  Compositor (PID 4)                      │
│  Neural Desktop, Window Manager, Omnibar, WASM Runtime  │
│  Damage Tracking, VirtIO-GPU 2D Rendering               │
├─────────┬──────────┬──────────┬────────────┬────────────┤
│ Intent  │  Shell   │ Synapse  │ Inference  │   WASM     │
│ Service │ (PID 3)  │ (PID 2)  │  Server    │   Apps     │
│ (PID 5) │ Commands │ SQLite   │  (PID 6)   │ (wasmi)   │
│         │ App Weav │ VFS      │  Qwen3     │            │
└────┬────┴────┬─────┴────┬─────┴─────┬──────┴────────────┘
     │         │          │           │
┌────▼─────────▼──────────▼───────────▼───────────────────┐
│                 Folkering Kernel                         │
│  SYSCALL/SYSRET, Preemptive Scheduler, APIC Timer       │
│  Per-Task Page Tables, SMP (4-core), HHDM Zero-Copy     │
│  IPC (send/reply), Shared Memory, SYS_MMAP/MUNMAP       │
├─────────────────────────────────────────────────────────┤
│                    VirtIO Drivers                        │
│  Block (DMA burst) │ Network (DHCP/DNS) │ GPU (2D)     │
│  Serial (COM1-3)   │ RDRAND entropy     │ RTC          │
└─────────────────────────────────────────────────────────┘
│                    Hardware (QEMU x86-64)                │
└─────────────────────────────────────────────────────────┘
```

## Kernel

The kernel provides the minimum required services for AI workloads:

| Component | Details |
|-----------|---------|
| **Scheduler** | Preemptive, timer IRQ-driven, priority-based with deadline support |
| **Syscalls** | SYSCALL/SYSRET with SWAPGS + per-CPU CpuLocal storage |
| **Memory** | Per-task PML4 page tables, HHDM mapping, SYS_MMAP/MUNMAP |
| **IPC** | Synchronous send/reply, async recv, CallerToken authentication |
| **Shared Memory** | Create, map, grant, unmap, destroy — correct page table insertion |
| **SMP** | 4-core via Limine SMP protocol, parallel GEMM dispatch |
| **Allocator** | Free-list allocator with coalescing (userspace), slab allocator (kernel) |
| **VirtIO Block** | DMA burst reads (64 sectors/request), FOLKDISK header format |
| **VirtIO Network** | DHCP, ICMP ping, DNS resolution via smoltcp |
| **VirtIO GPU** | Modern PCI Capabilities transport, 2D scanout, 1280x800 |
| **Serial** | COM1 (logging), COM2 (Gemini proxy), COM3 (God Mode Pipe) |

## AI Subsystem

### On-Device Inference
- **Model**: Qwen3-0.6B (Q4_0, 364MB GGUF) loaded via DMA burst from VirtIO disk
- **Quantization**: Q4_0/Q8_0 GEMM, Q6_K embeddings
- **Tokenizer**: BPE with merge priorities, special token handling, ChatML parity
- **Features**: RMSNorm, RoPE, SiLU-gated FFN, grouped-query attention, KV-cache
- **Performance**: 0.57s/tok with 4-core SMP parallel output projection

### Cloud AI (Hybrid)
- **Gemini 3 Flash** via COM2 serial proxy (bypasses QEMU SLIRP TCP issues)
- **Intent Engine**: JSON-RPC parsing for structured OS actions (move/close/resize windows)
- **WASM JIT Toolsmithing**: Gemini generates Rust code → proxy compiles to WASM → OS executes

### WASM Runtime (wasmi 0.38)
Two execution modes:
- **One-shot**: Compile + run + destroy. For tool scripts (draw a rectangle).
- **Persistent**: Store/Instance survive between frames. For interactive apps/games.

**12 Host Functions (Folk API):**

| Function | Signature | Purpose |
|----------|-----------|---------|
| `folk_draw_rect` | `(x, y, w, h, color)` | Filled rectangle |
| `folk_draw_line` | `(x1, y1, x2, y2, color)` | Bresenham line |
| `folk_draw_circle` | `(cx, cy, r, color)` | Midpoint circle outline |
| `folk_draw_text` | `(x, y, ptr, len, color)` | Text from WASM linear memory |
| `folk_fill_screen` | `(color)` | Fill entire framebuffer |
| `folk_get_time` | `() -> i32` | Uptime in milliseconds |
| `folk_screen_width` | `() -> i32` | Framebuffer width |
| `folk_screen_height` | `() -> i32` | Framebuffer height |
| `folk_random` | `() -> i32` | Hardware random (RDRAND) |
| `folk_poll_event` | `(ptr) -> i32` | Dequeue input event (16 bytes) |
| `folk_get_surface` | `() -> i32` | Zero-copy pixel buffer (stub) |

## Key Design Decisions

### No libc, No POSIX
Every syscall, every driver, every data structure is custom Rust. This eliminates legacy overhead and allows the kernel to be optimized specifically for AI workloads.

### HHDM Zero-Copy for SMP GEMM
Application Processors (APs) never switch CR3. The BSP translates userspace pointers to Higher-Half Direct Map addresses via page table walk. APs access weight tensors directly through HHDM. This avoids TLB flush overhead on 4 cores.

### COM2 Serial Backdoor
QEMU's SLIRP user-mode networking has TCP delivery issues under load. The serial proxy (`serial-gemini-proxy.py`) connects to COM2 via TCP socket, providing reliable bidirectional communication with the Gemini API. Protocol: `@@GEMINI_REQ@@{json}@@END@@` / `@@GEMINI_RESP@@{text}@@END@@`.

### Free-List Allocator
The original bump allocator never freed memory, causing OOM after 2-3 WASM executions. The replacement free-list allocator supports deallocation with address-sorted coalescing, enabling infinite consecutive WASM app loads.

### Fuel Metering
wasmi's fuel system limits each WASM execution to 1M instructions. In persistent mode, fuel resets each frame. This prevents infinite loops from freezing the compositor while still allowing complex per-frame logic.

## Directory Structure

```
folkering-os/
├── kernel/                    # x86-64 kernel
│   └── src/
│       ├── arch/x86_64/       # Syscalls, SMP, interrupts, paging
│       ├── drivers/           # VirtIO (block, net, GPU), serial, keyboard
│       ├── memory/            # PMM, page tables, heap
│       ├── net/               # smoltcp TCP/IP, TLS, DNS, Gemini client
│       ├── task/              # Scheduler, task management
│       └── ipc/               # IPC channels, shared memory
├── userspace/
│   ├── compositor/            # Window manager + WASM runtime
│   │   └── src/
│   │       ├── main.rs        # Compositor main loop (~3800 lines)
│   │       ├── wasm_runtime.rs # wasmi integration, Folk API
│   │       ├── intent.rs      # AI intent parser + base64 decoder
│   │       ├── graphics.rs    # Bresenham line, midpoint circle
│   │       ├── framebuffer.rs # Software rasterizer, WC optimization
│   │       ├── damage.rs      # Dirty rectangle tracker
│   │       ├── blend.rs       # Alpha blending
│   │       └── window_manager.rs # Windows, terminals, widgets
│   ├── shell/                 # Command execution, App Weaver
│   ├── synapse/               # SQLite VFS on VirtIO block
│   ├── inference-server/      # GGUF + transformer + sampling
│   ├── intent-bus/            # Capability routing
│   ├── libfolk/               # Syscall wrappers, IPC protocol
│   ├── libtensor/             # GEMM, RMSNorm, RoPE, KV-cache
│   └── libsqlite/             # no_std SQLite B-tree reader
├── tools/
│   ├── serial-gemini-proxy.py # COM2 Gemini API proxy + WASM compiler
│   ├── gemini-proxy.py        # HTTP proxy (legacy)
│   └── folk-pack/             # initrd packer + model injector
├── mcp/                       # MCP debug server (tensor_dump, attention_heatmap)
├── boot/                      # Limine config, disk images, model
├── LICENSE                    # AGPL-3.0 (dual licensing available)
├── CONTRIBUTING.md            # How to contribute + CLA requirement
└── ARCHITECTURE.md            # This file
```

## Building

```bash
# Prerequisites: Rust nightly, QEMU, Python 3.12, WSL2
cd kernel && cargo build --release    # Custom x86-64 target
cd userspace && cargo build --release # Custom x86-64 target

# Or use MCP server for full build→pack→boot cycle:
folkering_rebuild_run()
```

## Author

Knut Ingmar Merødningen — ikkjekvifull@gmail.com

## License

Folkering OS is dual-licensed:
- **AGPL-3.0**: Free for open-source use. See [LICENSE](LICENSE).
- **Commercial**: For proprietary/closed-source use. Contact ikkjekvifull@gmail.com.
