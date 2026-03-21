# Folkering OS — Claude Context

## Project Overview
Rust bare-metal x86-64 AI-native microkernel OS. Limine bootloader, QEMU for emulation, WSL build.

**Current milestone**: Epoch 1, M42 — First AI-generated tokens

## What Works (as of 2026-03-21)
- **On-device SLM inference**: SmolLM2-135M generates tokens via omnibar `ask` command
- Graphical desktop (Neural Desktop) with draggable terminal windows
- BPE tokenizer (Greedy Prefix Match), 30-layer transformer forward pass
- Top-P nucleus sampling with repetition penalty, KV-cache with sink eviction
- VirtIO block with multi-sector DMA bursting (ULTRA 36: 87MB in 45s)
- VirtIO network: DHCP, ICMP ping, DNS resolution
- SQLite VFS via Synapse on persistent VirtIO disk
- App Weaver: declarative UI → shmem → Compositor renders
- Full microkernel IPC: Compositor → Intent → Shell → Synapse → Inference

## 6 Userspace Tasks
| Task | Name | Purpose |
|------|------|---------|
| 1 | idle | Idle loop |
| 2 | synapse | SQLite, VFS, file cache |
| 3 | shell | Commands, WASM apps |
| 4 | compositor | GUI, windows, omnibar, terminal |
| 5 | intent-service | Capability-based IPC routing |
| 6 | inference | SLM inference (libtensor + GGUF) |

## Critical Lessons
1. **shmem_map MUST use task PML4** — `map_page_in_table(task.page_table_phys, ...)` NOT global MAPPER
2. **GGUF magic is 0x46554747** (not 0x46475547) — "GGUF" as LE u32
3. **Kernel mmap limit = 16MB/call** — inference server mmaps in 16MB chunks
4. **INFER_SHMEM_VADDR must not collide with MMAP_BASE** — use 0x20000000
5. **SmolLM2-135M Q4_0 uses Q8_0 for embeddings/output** — need dtype-aware GEMM
6. **VirtIO DMA: sector-by-sector = kernel DDoS** — multi-sector bursts essential
7. **Compositor omnibar catches all keyboard** — commands must be handled in compositor code

## Build
- Kernel: `kernel/` → `target/x86_64-folkering/release/kernel`
- Userspace: `userspace/` → `target/x86_64-folkering-userspace/release/{compositor,shell,synapse,intent-service,inference}`
- folk-pack: `tools/folk-pack/` — creates FPK initrd + packs GGUF models
- MCP build server: `C:\Users\merkn\folkering-mcp\server.py` (folkering_rebuild_run)
- MCP debug server: `mcp/server.py` (kernel_symbol_lookup, serial_throttle_analyzer, qemu_inspect_registers)

## FOLKDISK Header Layout (sector 0 of virtio-data.img)
```
[0..8]   "FOLKDISK" magic
[8..12]  version (u32)
[12..16] pad
[16..24] journal_start (u64)
[24..32] journal_size (u64)
[32..40] data_start (u64)
[40..48] data_size (u64)
[48..56] synapse_db_sector (u64)
[56..64] synapse_db_size (u64)
[64..72] model_sector (u64) — 4KB-aligned
[72..80] model_size (u64) — bytes
```

## Key Directories
| Path | Purpose |
|------|---------|
| `kernel/src/drivers/virtio_blk.rs` | VirtIO block + DMA burst |
| `userspace/libtensor/src/` | GEMM, GGUF, tokenizer, transformer, KV-cache |
| `userspace/inference-server/src/main.rs` | Model loading, forward pass, IPC |
| `userspace/compositor/src/main.rs` | GUI, omnibar, ask command handler |
| `tools/folk-pack/src/main.rs` | FPK + pack-model subcommand |
| `mcp/server.py` | Debug MCP (symbols, registers, serial) |

## Memory Layout
| Range | Owner |
|-------|-------|
| `0xffffffff80000000+` | Kernel (high-half) |
| `0x1_0000_0000` | Model mmap (87MB) |
| `0x4000_0000+` | Arena, KV-cache (mmap region) |
| `0x2000_0000` | Inference shmem IPC |
| `0x200000–0x4fffff` | Userspace code/data |
