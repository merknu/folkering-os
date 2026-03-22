# Folkering OS — Development Changelog

> A bare-metal Rust x86-64 operating system with on-device AI inference.
> Built from scratch — no Linux, no libc, no std.

---

## Day 1 — 23. januar 2026
**Grunnmuren: Microkernel fra scratch**

- `478b24e` Initial commit: microkernel med user-mode support
- Monorepo-struktur, ISR, syscall handler, GDT/IDT
- User-mode task execution med zero-stack creation
- IPC message passing via register-baserte syscalls
- 32KB kernel stack aktivert
- Serial driver, QEMU boot-infrastruktur

---

## Day 2-3 — 24.-26. januar 2026
**AI-Native Arkitektur**

- `1ac022b` AI-Native OS branch setup
- Synapse: Neural Knowledge Graph Filesystem (SQLite-basert VFS)
- Intent Bus med Semantic Routing
- Neural Scheduler med CPU frequency scaling
- WASM Runtime for applikasjoner
- IPC milestone: 635 round-trips på 15 sekunder

---

## 18. mars 2026
**Grafisk brukergrensesnitt**

- `3ad45a7`–`e7b92c9` Milestone 4-4.5: Compositor med vindushåndtering
- SYS_MUNMAP, UI wire protocol
- Native UI Schema — apper rendret via IPC
- Interaktive knapper med event dispatch
- Framebuffer-basert rendering med skygger og tittellinjer

---

## 19. mars 2026
**Persistent filsystem**

- `047fe5b` VFS Write — persistent filskriving via SQLite cell append

---

## 20. mars 2026
**Nettverk og GitHub**

- `3723e46` Network stack: ICMP ping, DNS, RTC, RNG, TLS 1.3
- `5be3eb4` GitHub API, JSON parser, clone-to-VFS

---

## 21. mars 2026
**Epoch 1: AI-hjernen snakker sine første ord**

| Commit | Tid | Hva |
|--------|-----|-----|
| `7a763a5` | 08:25 | Epoch 1 — SmolLM2-135M inference engine (M33-M42) |
| `3899de8` | 17:07 | Async token streaming, ChatML template, Q8_0 fixes |
| `580881d` | 18:12 | Layer-by-layer debugging med Python referanse |
| `52873fc` | 19:12 | **Fix:** Zero Q/K/V buffere — fikset akkumulert projeksjonsfeil |
| `a0454a1` | 22:04 | **Double Fault fikset:** Q4_0 nibble-rekkefølge + tokenizer special tokens |
| `8877e7d` | 23:02 | Differensial tokenizer-fuzzer (5000 test-cases) |
| `7406a7d` | 23:15 | Fjernet debug-dumps, ren kodebase |

**Resultat:** Modellen produserte gjenkjennelige engelske ord for første gang.
Output gikk fra komplett gibberish til sammenhengende fraser.

---

## 22. mars 2026
**Roadmap to Perfection — fra 22% til 98.7% tokenizer-parity**

### Morgen: BPE Tokenizer & Visual Inspection Studio

| Commit | Tid | Hva |
|--------|-----|-----|
| `e893876` | 00:00 | **Proper BPE tokenizer** — 48,900 merge-regler, FNV-1a hash, binary search |
| `cd69424` | 11:35 | **Visual Inspection Studio** — attention heatmap med Rust/Python drift-modus |

**BPE-oppgradering:**
- Erstattet greedy longest-prefix-match med ekte BPE med merge-prioriteter
- FNV-1a hash-tabell (512KB, temporary) for vocab string→ID lookup
- Special token handling: `<|im_start|>` → token 1, `<|im_end|>` → token 2
- ChatML-parity: 24 tokens (identisk med HuggingFace), ned fra 55

**Visual Inspection Studio:**
- 128KB VirtIO disk mailbox (256 sektorer)
- Post-softmax attention dump for alle 9 heads
- MCP `attention_heatmap` viser DRIFT mellom bare-metal Rust og PyTorch

### Ettermiddag: Pre-tokenizer & Control Sector

| Commit | Tid | Hva |
|--------|-----|-----|
| `0767825` | 13:38 | **GPT-2 pre-tokenizer** — word boundaries, contractions, whitespace-as-prefix |
| `c2f779d` | 14:19 | Lowercase-only contractions + newline boundary regler |
| `95258e3` | 14:24 | **VirtIO Control Sector (258)** — konfigurerbar sampling uten rekompilering |

**Pre-tokenizer:**
- Ord bærer leading space: `"hello world"` → `["hello", " world"]`
- Contractions splittes: `"don't"` → `["don", "'t"]`
- Unified whitespace handler: 1 regel for alle ws-typer

**Control Sector:**
- `set_control(temperature=0.3, dump_layer=15, top_k=50)` via MCP
- SamplingConfig struct: temperature, top_p, top_k, rep_penalty, rep_window, dump_layer
- Zero-recompile konfigurering — tar effekt på neste inference request

### Sen ettermiddag: 100% ASCII Parity

| Commit | Tid | Hva |
|--------|-----|-----|
| `952535d` | 15:43 | **100% ASCII parity** — 0 failures for all printable ASCII |
| `1912f9b` | 16:20 | **UTF-8 byte fallback** — 98.8% total parity |
| `9e1b699` | 16:30 | Stabil 98.7% med lenient UTF-8 dekoding |

**Tokenizer-parity progression:**

```
22%  ████░░░░░░░░░░░░░░░░  Greedy LPM (før BPE)
47%  █████████░░░░░░░░░░░  Etter BPE upgrade
50%  ██████████░░░░░░░░░░  Med pre-tokenizer
98.7% ███████████████████░  Med UTF-8 byte fallback
100%  ████████████████████  ASCII/Unicode/ChatML (all text)
```

### Kveld: P4 Activation Monitor & P5 Streaming Hardening

| Commit | Tid | Hva |
|--------|-----|-----|
| `85d17d0` | 18:03 | **P4: Activation Monitor** — MSE health telemetry med "Check Engine" lys |
| `ed0b980` | 18:40 | **P5: Streaming hardening** — KV-cache guard, shmem safety, typewriter cursor |

**Activation Monitor:**
- MSE mellom konsekutive logits detekterer hallusinasjon/looping
- 3 telemetri-moduser: Off / Anomalies Only / Continuous
- `drift_threshold` konfigurerbar via Control Sector
- `min_mse` tracking — "Check Engine" lys som husker verste punkt
- `read_activation_health()` MCP-verktøy

**Streaming Hardening:**
- KV-cache overflow guard: `[Context Limit Reached]` ved pos >= 256
- Recycled window ID protection: forhindrer token-hijacking
- Typewriter cursor: 8x8 blokk-markør vises under generering

---

## Arkitektur-oversikt (per 22. mars 2026)

```
┌─────────────────────────────────────────────────────────┐
│                    FOLKERING OS                          │
│                 Bare-Metal x86-64                        │
├─────────────────────────────────────────────────────────┤
│  Compositor         │  Inference Server                  │
│  - Window Manager   │  - SmolLM2-135M (Q4_0)            │
│  - TokenRing poll   │  - BPE Tokenizer (98.7% parity)   │
│  - Typing cursor    │  - Top-P + Top-K + Rep Penalty     │
│  - AccessKit UI     │  - Async TokenRing streaming       │
│                     │  - MSE Health Monitor               │
├─────────────────────┼────────────────────────────────────┤
│  VirtIO Disk                                             │
│  Sector 0:     FOLKDISK header                           │
│  Sector 1-257: Tensor mailbox (128KB)                    │
│  Sector 258:   Control Sector (FCTL)                     │
│  Sector 259:   Health Telemetry (HLTH)                   │
│  Sector 2048+: SQLite DB + GGUF Model                    │
├─────────────────────────────────────────────────────────┤
│  MCP Tools (Host Python)                                 │
│  - tensor_dump, attention_heatmap, topo_parity_map       │
│  - python_ref_runner (PyTorch oracle)                    │
│  - set_control, read_activation_health                   │
├─────────────────────────────────────────────────────────┤
│  Kernel: Microkernel, SysV ABI, BumpArena allocator      │
│  Network: ICMP, DNS, TLS 1.3                             │
│  Storage: SQLite VFS, GGUF model loader                  │
└─────────────────────────────────────────────────────────┘
```

---

## Nøkkeltall

| Metrikk | Verdi |
|---------|-------|
| Total commits | 60+ |
| Utviklingsperiode | 23. januar – 22. mars 2026 |
| Kernel | Rust no_std, x86-64, Limine bootloader |
| Modell | SmolLM2-135M, Q4_0 kvantisering |
| Tokenizer parity | 98.7% total, 100% for all text |
| Vocab size | 49,152 tokens |
| BPE merge rules | 48,900 |
| KV-cache | 256 tokens, 11.7MB |
| Mailbox | 128KB (256 sektorer) |
| Arena | 8MB BumpArena |
| MCP tools | 9 verktøy for debugging/konfigurering |
