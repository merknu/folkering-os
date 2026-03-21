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
- MCP debug server: `mcp/server.py` v2.0 — see "ML Inspection Studio" section below

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
| `mcp/server.py` | Debug MCP v2.0 (symbols, registers, serial, **tensor_dump**, **python_ref_runner**) |

## Memory Layout
| Range | Owner |
|-------|-------|
| `0xffffffff80000000+` | Kernel (high-half) |
| `0x1_0000_0000` | Model mmap (87MB) |
| `0x4000_0000+` | Arena, KV-cache (mmap region) |
| `0x2000_0000` | Inference shmem IPC |
| `0x200000–0x4fffff` | Userspace code/data |

## ML Inspection Studio (MCP Debug Server v2.0)

`mcp/server.py` registered as `folkering-debug` (py -3.12). Five tools:

### Original tools
- `kernel_symbol_lookup(addresses, elf_path?)` — resolve hex addr → function name
- `serial_throttle_analyzer(log_path, ...)` — collapse loop patterns in serial logs
- `qemu_inspect_registers(include_xmm?, qmp_socket?)` — CPU register dump via QMP

### Tool: `tensor_dump` — Read Tensor Data from Inference Server

The Rust inference-server writes tensor stats + raw f32 data to VirtIO disk sectors 1-7 after each forward pass. This tool reads them directly from the host disk image file.

**When to use:** After running inference in QEMU, to inspect logits, hidden states, or any tensor the inference-server dumped.

**Rust-side dump points (in inference-server/src/main.rs):**
- `bos_logits` — after processing BOS token (token 0)
- `prefill_final_logits` — after last prefill token (used for sampling)
- Add custom: `debug_dump_logits(logits, "my_name")` or `debug_dump_hidden(data, "my_name")`

**Usage patterns:**
```
# Quick stats check (no raw data)
tensor_dump()

# Get raw float values + top-20 logits
tensor_dump(return_data=true, top_k=20)

# Specific slice of tensor data
tensor_dump(return_data=true, slice_start=100, slice_end=200)

# Parse from serial log instead of disk
tensor_dump(serial_log="/tmp/folkering-serial.log", name_filter="prefill_final_logits")
```

**Disk mailbox layout:** Sector 1 = header (magic "TDMP", seq, shape, stats, name, first 100 f32), Sectors 2-7 = raw f32 data (max 768 values).

### Tool: `python_ref_runner` — PyTorch Ground-Truth Oracle (ULTRA 50)

Loads SmolLM2-135M via HuggingFace `transformers` + PyTorch. Model stays in memory after first load. Use as ground truth for comparing Rust inference output.

**When to use:** When debugging transformer divergence, GQA bugs, or precision issues. Compare layer-by-layer with Rust output.

**Modes:**
- `logits` — top-K logits at last token position (default, most common)
- `generate` — generate N tokens
- `tokens` — show tokenization
- `compare` — auto-reads Rust tensor_dump from disk and computes element-wise diff

**Usage patterns:**
```
# Reference logits (greedy, deterministic)
python_ref_runner(prompt="Hello", mode="logits", top_k=20)

# Compare with Rust (reads disk mailbox automatically)
python_ref_runner(prompt="<|im_start|>system\n...", mode="compare")

# Capture layer 0 Q projection (forward hook)
python_ref_runner(prompt="Hello", layer=0, module_name="self_attn.q_proj")

# Capture multiple activations at once
python_ref_runner(prompt="Hello", capture_layers=[
    "model.layers.0.self_attn.q_proj",
    "model.layers.0.self_attn.k_proj",
    "model.layers.0.self_attn.v_proj",
    "model.layers.0.self_attn",
    "model.layers.1.self_attn"
])
```

**Available module names for `capture_layers` / `module_name`:**
- Attention: `self_attn.q_proj`, `self_attn.k_proj`, `self_attn.v_proj`, `self_attn.o_proj`, `self_attn`
- FFN: `mlp.gate_proj`, `mlp.up_proj`, `mlp.down_proj`, `mlp`
- Norms: `input_layernorm`, `post_attention_layernorm`

**SmolLM2-135M architecture (for reference):**
- 30 layers, embed_dim=576, n_heads=9, n_kv_heads=3 (GQA 3:1), head_dim=64
- intermediate_size=1536, vocab=49152, context=2048
- Q shape: [1, seq, 576], K/V shape: [1, seq, 192]

### Debugging workflow for transformer divergence

1. Run inference in QEMU → generates `[TDMP]` serial lines + disk mailbox
2. `tensor_dump(top_k=20)` — see Rust top logits
3. `python_ref_runner(prompt=SAME_PROMPT, mode="compare")` — auto-diff
4. If argmax diverges: narrow down the layer
5. For each layer L: `python_ref_runner(prompt=X, layer=L, module_name="self_attn.q_proj")` — get Python Q
6. Compare with Rust serial output for layer L
7. When divergence layer found: check GEMM, RoPE, attention score computation

### Dependencies (py -3.12)
torch 2.10.0+cpu, transformers 5.3.0, numpy 2.4.3, llama-cpp-python 0.3.16
