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
│         │ App Weav │ VFS      │  Qwen3     │ 18+ apps  │
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
| **VirtIO Block** | DMA burst reads (64 sectors/request), FOLKDISK header format |
| **VirtIO Network** | DHCP, ICMP ping, DNS resolution, TLS 1.3 via smoltcp |
| **VirtIO GPU** | Modern PCI Capabilities transport, 2D scanout, 1280x800 |
| **Serial** | COM1 (logging), COM2 (Gemini proxy), COM3 (God Mode Pipe) |
| **Telemetry** | 8192-event ring buffer, IQE latency tracking, 12 action types |
| **WebSocket** | RFC 6455 client, 4 connection slots, 8KB recv ring buffer |

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
- **WASM JIT Toolsmithing**: Gemini generates Rust code -> proxy compiles to WASM -> OS executes

### AutoDream Self-Improvement
- **Pattern Mining**: Drains telemetry ring buffer, sends to LLM for insight extraction
- **Creative Mode**: LLM generates improved WASM variants of existing apps
- **Nightmare Mode**: LLM generates adversarial test cases (crash/hang fuzzing)
- **Refactor Mode**: LLM refactors WASM apps based on usage patterns
- **Shadow Runtime**: Sandboxed WASM testing with mocked host functions

## WASM Runtime (wasmi 2.0)

Two execution modes:
- **One-shot**: Compile + run + destroy. For tool scripts.
- **Persistent**: Store/Instance survive between frames. For interactive apps/games.

### Host Functions (Folk API) — 47 functions across 5 modules

#### Graphics (host_api/graphics.rs)
| Function | Signature | Purpose |
|----------|-----------|---------|
| `folk_draw_rect` | `(x, y, w, h, color)` | Filled rectangle |
| `folk_draw_text` | `(x, y, ptr, len, color)` | Text from WASM linear memory |
| `folk_draw_line` | `(x1, y1, x2, y2, color)` | Bresenham line |
| `folk_draw_circle` | `(cx, cy, r, color)` | Midpoint circle outline |
| `folk_fill_screen` | `(color)` | Fill entire framebuffer |
| `folk_get_surface` | `() -> i32` | Zero-copy pixel buffer offset |
| `folk_surface_pitch` | `() -> i32` | Bytes per row |
| `folk_surface_present` | `()` | Mark surface dirty for blit |
| `folk_draw_pixels` | `(x, y, w, h, ptr, len) -> i32` | Raw RGBA pixel blit (images) |
| `folk_submit_display_list` | `(ptr, len) -> i32` | Batch rendering (1000x fuel reduction) |

#### Network (host_api/network.rs)
| Function | Signature | Purpose |
|----------|-----------|---------|
| `folk_http_get` | `(url_ptr, url_len, buf, max) -> i32` | HTTP GET via proxy (8KB) |
| `folk_http_get_large` | `(url_ptr, url_len, buf, max) -> i32` | Large HTTP GET (256KB) |
| `folk_ws_connect` | `(url_ptr, url_len) -> i32` | WebSocket connect |
| `folk_ws_send` | `(socket, data_ptr, len) -> i32` | WebSocket send frame |
| `folk_ws_poll_recv` | `(socket, buf, max) -> i32` | WebSocket non-blocking receive |

#### AI & Inference (host_api/ai.rs)
| Function | Signature | Purpose |
|----------|-----------|---------|
| `folk_slm_generate` | `(prompt_ptr, len, buf, max) -> i32` | LLM generate (local + proxy fallback) |
| `folk_slm_generate_with_logits` | `(prompt, len, out, max) -> i32` | Generate + PLAB logit analysis |
| `folk_intent_fetch` | `(query, len, buf, max) -> i32` | Semantic network request |
| `folk_tokenize` | `(text, len, out, max) -> i32` | BPE-style tokenization |
| `folk_tensor_read` | `(buf, len, sector) -> i32` | Read TDMP tensor mailbox |
| `folk_tensor_write` | `(sector, offset, value) -> i32` | Write TDMP tensor data |
| `folk_semantic_extract` | `(html, len, buf, max) -> i32` | AI semantic web extraction |

#### VFS (host_api/vfs.rs)
| Function | Signature | Purpose |
|----------|-----------|---------|
| `folk_request_file` | `(path, len, dest, max) -> i32` | Async file load (handle-based) |
| `folk_query_files` | `(query, len, result, max) -> i32` | Semantic file query |
| `folk_list_files` | `(buf, max) -> i32` | Directory listing |
| `folk_write_file` | `(path, len, data, len) -> i32` | Write to Synapse VFS |
| `folk_read_file_sync` | `(path, len, dest, max) -> i32` | Synchronous file read |

#### System (host_api/system.rs)
| Function | Purpose |
|----------|---------|
| `folk_get_time` | Uptime in milliseconds |
| `folk_screen_width/height` | Framebuffer dimensions |
| `folk_random` | Hardware random (RDRAND) |
| `folk_get_datetime` | RTC date/time (6 x i32) |
| `folk_os_metric` | Live system metrics |
| `folk_net_has_ip` | Network connectivity check |
| `folk_fw_drops` | Firewall drop count |
| `folk_poll_event` | Dequeue input event (16 bytes) |
| `folk_log_telemetry` | Push event to kernel ring buffer |
| `folk_telemetry_poll` | Drain telemetry events |
| `folk_pci_list` | PCI device enumeration |
| `folk_irq_stats` | IRQ/driver statistics |
| `folk_memory_map` | Memory stats + heatmap |
| `folk_ipc_stats` | Task list with IPC activity |
| `folk_shadow_test` | Run WASM in shadow sandbox |
| `folk_stream_write/read/done` | Semantic Streams (Tick-Tock) |

## Compositor Architecture

The compositor is the heart of the UI. After refactoring, it follows a modular architecture:

```
compositor/src/
├── main.rs              1072 lines  Init + main loop skeleton
├── rendering.rs         1708 lines  Desktop, WASM fullscreen, present
├── command_dispatch.rs  2243 lines  Omnibar + terminal commands
├── mcp_handler.rs       1518 lines  MCP polling, AutoDream, Draug, streaming
├── input_keyboard.rs     691 lines  Keyboard event handling
├── input_mouse.rs        553 lines  Mouse, drag, hit-testing
├── ipc_helpers.rs        436 lines  IPC messages, tool execution, widgets
├── state.rs              383 lines  7 typed state structs
├── ui_dump.rs            252 lines  UI state JSON serialization
├── util.rs               191 lines  Formatting, intent matching
├── allocator.rs          164 lines  FreeListAllocator (16MB heap)
├── iqe.rs                 94 lines  Input latency telemetry
├── god_mode.rs            31 lines  COM3 command injection
├── wasm_runtime.rs      1041 lines  wasmi integration, execution modes
├── host_api/
│   ├── mod.rs             17 lines  Module declarations + re-exports
│   ├── graphics.rs       248 lines  10 drawing host functions
│   ├── network.rs        170 lines  5 HTTP/WebSocket host functions
│   ├── ai.rs             403 lines  7 AI/inference host functions
│   ├── vfs.rs            201 lines  5 file system host functions
│   └── system.rs         374 lines  19 system host functions
├── (existing modules)
├── framebuffer.rs               Software rasterizer, WC optimization
├── damage.rs                    Dirty rectangle tracker
├── window_manager.rs            Windows, terminals, widgets
├── draug.rs                     AutoDream daemon
├── folkshell.rs                 FolkShell command pipeline
├── spatial.rs                   Spatial pipelining (node graph)
├── driver_runtime.rs            Autonomous WASM driver runtime
├── blend.rs                     Alpha blending
├── font.rs                      Bitmap font
├── graphics.rs                  Bresenham line, midpoint circle
├── intent.rs                    AI intent parser
├── slm_runtime.rs               On-device SLM brain
└── lib.rs                       Compositor library (WorldTree, types)
```

### Main Loop Flow (main.rs)

```
fn main() -> ! {
    // 1. Init: boot info, framebuffer, GPU, state structs, categories
    // 2. Initial desktop render (title, omnibar, status bar)
    // 3. Window manager + Draug daemon + MCP client init

    loop {
        // Per-frame timing (RDTSC)
        // Clock tick: targeted status bar render (50us, no full redraw)

        // AI Systems (mcp_handler.rs)
        tick_ai_systems()     // WASM JIT, agent, Draug, AutoDream, MCP polling

        // COM3 God Mode (god_mode.rs)
        poll_com3()           // Serial command injection

        // IQE Telemetry (iqe.rs)
        poll_telemetry()      // Keyboard/mouse latency EWMA

        // Input (input_mouse.rs, input_keyboard.rs)
        process_mouse()       // Cursor, drag, hit-test, WASM events
        process_keyboard()    // Text input, commands, special keys

        // Command Dispatch (command_dispatch.rs)
        dispatch_omnibar()    // FolkShell, open, gemini, agent, dream, ...

        // Rendering (rendering.rs)
        render_frame()        // Semantic Streams, WASM, desktop, windows
        present_and_flush()   // Shadow->FB, GPU flush, VGA mirror

        // IPC + Streaming (mcp_handler.rs)
        tick_ipc_and_streaming()  // IPC messages, TokenRing, think overlay

        // Timing report + idle spin
    }
}
```

## folk_browser — AI-Native Web Browser

The browser (`apps/folk_browser/`) is a WASM app with two viewing modes:

- **Standard View**: HTML parser -> box model layout -> display list rendering
- **Semantic View**: Raw HTML -> `folk_semantic_extract` -> LLM-powered content extraction -> clean markdown

### Image Support
| Format | Decoder | Method |
|--------|---------|--------|
| PNG | Full DEFLATE inflate + row filters | Pixel-accurate |
| JPEG | Marker parsing + color sampling | Gradient preview |
| GIF | Full LZW decompression | Pixel-accurate (first frame) |
| WebP | VP8/VP8L dimension extraction | Gradient preview |

## WASM Apps (18+)

| App | Size | Category |
|-----|------|----------|
| folk_browser | 22KB | Web browsing with Semantic Toggle |
| polyglot_chat | 24KB | Multi-model AI chat |
| kernel_snoop | 14KB | Kernel diagnostics |
| semantic_mail | 8KB | AI email composer |
| wasm_forge | 8KB | WASM code generator |
| tensor_view | 7KB | Tensor visualization |
| folk_test_runner | 7KB | Automated regression |
| prompt_lab | 7KB | Prompt engineering |
| saliency_mapper | 7KB | Attention visualization |
| context_weaver | 6KB | Semantic search |
| vfs_explorer | 6KB | File system browser |
| async_flow | 5KB | IPC bus visualizer |
| weight_wrangler | 5KB | Model weight editor |
| auto_doc | 5KB | Auto documentation |
| driver_studio | 4KB | PCI & hardware dashboard |
| dream_state | 4KB | AutoDream monitor |
| bpe_analyzer | 3KB | Tokenizer inspector |
| slab_visualizer | 2KB | Memory allocator view |

## Key Design Decisions

### No libc, No POSIX
Every syscall, every driver, every data structure is custom Rust. This eliminates legacy overhead and allows the kernel to be optimized specifically for AI workloads.

### HHDM Zero-Copy for SMP GEMM
Application Processors (APs) never switch CR3. The BSP translates userspace pointers to Higher-Half Direct Map addresses via page table walk. APs access weight tensors directly through HHDM. This avoids TLB flush overhead on 4 cores.

### COM2 Serial Backdoor
QEMU's SLIRP user-mode networking has TCP delivery issues under load. The serial proxy (`serial-gemini-proxy.py`) connects to COM2 via TCP socket, providing reliable bidirectional communication with the Gemini API.

### Free-List Allocator
The original bump allocator never freed memory, causing OOM after 2-3 WASM executions. The replacement free-list allocator (allocator.rs) supports deallocation with address-sorted coalescing, enabling infinite consecutive WASM app loads.

### Fuel Metering
wasmi's fuel system limits each WASM execution to 1M instructions (5M for foreground apps). This prevents infinite loops from freezing the compositor while still allowing complex per-frame logic.

### Modular Compositor
The compositor was refactored from two monolithic files (7723 + 2453 lines) into 18 focused modules. Each module takes only the state structs it needs as `&mut` parameters, keeping borrow checker happy and dependencies explicit.

## Building

```bash
# Prerequisites: Rust nightly (2026-01-20), QEMU, Python 3.12, WSL2
cd kernel && cargo build --release    # Custom x86-64 target
cd userspace && cargo build --release # Custom x86-64 target

# Or use MCP server for full build->pack->boot cycle:
folkering_rebuild_run()
```

## Author

Knut Ingmar Merodningen -- ikkjekvifull@gmail.com

## License

Folkering OS is dual-licensed:
- **AGPL-3.0**: Free for open-source use. See [LICENSE](LICENSE).
- **Commercial**: For proprietary/closed-source use. Contact ikkjekvifull@gmail.com.
