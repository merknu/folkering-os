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

## 12.-13. april 2026
**Draug Autonomous Evolution — Phase 13-16 + Silverfir-nano JIT**

### Phase 13: Overnight Code Loop
- Draug writes Rust autonomously via Gemma4 LLM gateway (Ollama)
- 177/177 iterations PASS, 0 fail — pipeline reliability proven
- 20 math tasks (fib, gcd, is_prime, collatz, binary_search, etc.)
- Host-side sandbox: `cargo test` + archive

### Phase 14: Skill Tree (L1→L2→L3)
- L1 (The Fixer): write function — 20/20 pass
- L2 (TDD): function + `#[cfg(test)]` with 3+ tests — 20/20 pass
- L3 (Evolution): optimize with prior code from MemPalace — 20/20 pass
- **Error-driven retry**: compiler errors fed back to LLM (max 2 retries)
- Boot persistence: 26-byte state to Synapse, resumes after restart
- Model selection: `qwen2.5-coder:7b` for L1 (4x faster), `gemma4:31b-cloud` for L2+

### Phase 15: Plan-and-Solve
- Planner persona breaks complex tasks into `STEP|description` steps
- Executor builds code incrementally with context chaining
- 4/8 complex tasks COMPLETE: SPSC ringbuffer, bump allocator, bitset, task queue
- Knowledge graph tracks plans as TODO_STEP entities

### Phase 16: WASM Deploy Pipeline
- New `draug-wasm-sandbox/` crate (wasm32-unknown-unknown target)
- Proxy `WASM_COMPILE` command: compiles Rust to .wasm binary
- Kernel syscall 0x63 `sys_wasm_compile`: returns .wasm bytes to OS
- Verified: 79-byte fib.wasm compiled end-to-end
- **THE LOOP IS CLOSED**: deploy_wasm now executes code via silverfir JIT
  - Draug writes code → cargo test → WASM compile → silverfir JIT → RUNS IN OS
  - First time autonomously generated code executes inside the running OS

### Silverfir-nano JIT (merged into ai-native-os)
- `WasmBackend` enum: `Sandboxed` (wasmi) vs `Trusted` (silverfir JIT)
- WASM parser: type, import, function, export, code sections
- x86_64 translator: i32 arithmetic, comparisons, locals, control flow
- W^X memory: `sys_mprotect` (syscall 0x32) enforces Write XOR Execute
- **SELF-TEST PASS**: WASM `(i32.const 42)` → 15 bytes x86_64 → native execution → returned 42

### TCP Remote Shell (port 2222)
- Commands: help, ps (with state), uptime, mem, net, df, ping, clear
- `draug status`: skill tree, iteration count, pass/fail/skip, current task, success rate
- `draug pause/resume`: remote control via kernel atomic bridge (syscall 0xD0/0xD1)
- Character echo + backspace, auto re-listen on disconnect
- Firewall whitelist for port 2222 inbound SYN
- Shell responsive during blocking LLM calls (polled from tcp_plain loops)
- Also ported to folkering-daq (Pi 5)

### 9 Stability Fixes
- Model selection: L1 uses fast 7b, L2+ uses 31b
- Boot persistence: skip KHunt on restore (saves 30-60s per boot)
- Adaptive backoff: exponential 60→120→240→300s on Ollama downtime
- Hibernation: pause after 30 consecutive skips
- Proxy PING: 2s fast-fail health check (cached 60s)
- LLM SKIP: Ollama errors don't count as task failures
- Heap monitoring: memory_stats every 50 iters, pause at 80%
- Error memory: task_errors[20] fed into next iteration's prompt
- Proxy auto-retry Chrome + Ollama keepalive (ping every 4 min)

### Security Hardening
- Source code scanner: blocks `std::process`, `std::fs`, `include!`, etc.
- URL allowlist: NAVIGATE only allows http(s), blocks internal IPs (SSRF)
- Ground-truth test injection: hardcoded expected values for 10 functions
- Tautology detection: rejects `assert_eq!(f(x), f(x))`
- Mutation testing: mutates code, verifies tests catch it

### Code Quality Audits
- Atomic memory ordering: all Draug bridge atomics fixed to Acquire/Release
- Heap fragmentation: fixed retry buffers, explicit plan cleanup, capped allocations
- Iter count on skip: advance_refactor moved after proxy check (prevents permanent lockout)
- L1 code persistence: task_code saved to Synapse for L3 context after boot
- Abandoned task guard: execute_next_step checks plan.completed
- All proxy TCP timeouts reduced from 900K to 120K tsc_ms (WCET 16min→4min)

### Async TCP — Zero UI Freeze (EAGAIN State Machine)
- New kernel module `tcp_async.rs`: 4 non-blocking syscalls (0xE0-0xE3)
  - `sys_tcp_connect` → slot_id or EAGAIN
  - `sys_tcp_send` → bytes or EAGAIN
  - `sys_tcp_poll_recv` → bytes, EAGAIN, or 0 (EOF)
  - `sys_tcp_close` → free slot
- `draug_async.rs`: Full async state machine for ALL Draug operations
  - Skill tree L1-L3: fully async (<1ms per frame)
  - Phase 15 Plan-and-Solve: fully async (<1ms per frame)
  - Before: UI froze 3-80s per LLM call. After: NEVER freezes
- 90-second timeout per async phase (prevents permanent hang)
- 6 parse robustness fixes: fail_count for executor, overflow guard, etc.

### TCP Shell Hardening
- Idle client timeout: 5 minutes, frees socket for other users
- recv error: proper disconnect instead of silent `unwrap_or(0)`
- Buffer overflow: warns user "(line truncated at 255 bytes)"
- All `line_buf` access verified bounds-safe

### Synapse VFS Eviction
- Removed unbounded knowledge graph writes from Phase 15
- Before: entities + edges grew ~113 KB/day → 4 MB DB full in ~4 weeks
- After: steady-state ~400 KB (10% of 4 MB) → never fills up
- Bounded data only: draug_state.bin, draug_code_N.rs, driver WASM

### Silverfir-nano JIT Pool
- `JitPool`: 1 MB pre-allocated region with bitmap allocation
- Eliminates mmap/munmap churn from repeated compilations
- `dealloc()`: zeroes pages with INT3 (0xCC), clears bitmap
- No memory leak possible: bitmap is single source of truth

### Commits
```
ai-native-os branch (20 commits):
  29adabf  Phase 13-16 + TCP remote shell
  9db1ea1  Ping crash fix, skip KHunt, bridge after restore
  e76a97e  Cached proxy ping, adaptive interval, net/df
  aeb6a2a  Shell improvements — draug current, clear, remove traceroute
  d90f99e  Audit: try_lock, ping cache, bridge rate-limit
  c4596f4  Audit: iter count, L1 persistence, abandoned task guard
  4933684  Heap fragmentation fixes
  bdd8457  Atomic memory ordering (Acquire/Release)
  c88ce9d  Changelog (initial)
  8d6ed28  Non-blocking async TCP syscalls (EAGAIN)
  a5f2ecd  Draug async state machine types
  1bdb0b5  Non-blocking Draug iterations
  8721ec4  Remove blocking fallback
  ce2a0a6  Phase 15 fully async
  83a713d  6 parse robustness fixes
  38d1c7d  TCP shell hardening
  1cd3b70  Async TCP timeout (90s)
  308b0f1  Remove unbounded graph writes

silverfir-nano-wasm-hybrid branch (9 commits):
  73cd49b  WasmBackend dual-runtime enum
  b51c1ea  JIT scaffold + CodeBuffer
  e21de43  W^X memory + protect_in_table
  57019be  WASM parser + x86_64 translator
  48d16c1  Unsigned comparisons + load/store
  f5bac38  JIT self-test PASS (compilation)
  933e18b  Native execution — returned 42
  800037c  All TCP timeouts reduced to 120K
  faba2df  JitPool 1MB bitmap allocation

folkering-proxy (6 commits):
  5d9a6b3  Rust rewrite + Phase 13-16
  9f58b0a  WASM panic handler fix
  a1feef6  Strip #[cfg(test)] for WASM
  77f0010  Ground-truth test injection
  34de80a  Tautology detection + mutation testing
  fb012c0  Security: sandbox escape + SSRF prevention

folkering-daq (1 commit):
  372a222  TCP remote shell port 2222
```

---

## 15. april 2026
**Hardware sprint: MSI-X, NVMe, DMA-pool, MVFS-på-NVMe**

### MSI-X interrupt routing
- `drivers/msix.rs` — capability walker (PCI cap 0x11), vector allocator
  for IDT 64-95, MMIO-mapping table locator (NO_CACHE flags), entry
  programmer. VirtIO-blk migrated from IRQ11/IOAPIC til MSI-X vektor 64.
- Self-test + KGraph + MVFS-round-trip bekreftet end-to-end på MSI-X-banen.
- Oppdaget + fikset: `arch/x86_64/idt.rs`'s lazy_static IDT er inaktiv
  (kernel bruker main.rs' manuelt oppsatte IDT). Naked asm-stubs for vektor
  64 + 65 lagt til der.

### NVMe driver (`drivers/nvme.rs`)
- PCI class-0x01/0x08/0x02 detect, BAR0 MMIO (NO_CACHE), CAP/VS/CC/CSTS
  handshake, admin + I/O queue pair med phase-tag polling.
- Identify Controller + Namespace (QEMU: 32768 LBAs × 512 B = 16 MiB).
- PRP1 / PRP1+PRP2 / PRP-list transfers opp til 63 datasider per kommando
  (~252 KiB). Alle tre moduser verifisert med self-test.
- Write/read round-trip (0xDEADBEEF på LBA 1) + flerblokk multi-PRP-test
  (8/16/32 blokker).

### DMA-side-pool (Phase 4)
- 64 forhåndsallokerte 4 KiB-sider med `AtomicU64` fri-bitmap (CAS-basert
  acquire, fetch_or release). Zero allocations på hot path.
- `lease_pages()` med error-unwind — partielle leases slippes ved feil.
- Leak-sjekk: 64/64 sider frie etter self-test bekrefter korrekt release.

### Completion-timeout + CSTS.CFS watchdog
- `submit_and_wait` bundet til 500M iter med periodisk Controller Fatal
  Status-sjekk. Wedget controller → ren feil, aldri kernel-hang.

### Interrupt-drevet completion (hybrid wait)
- 1M-iter spin-pause budsjett (~300 μs) deretter `hlt` mellom fase-sjekker.
- MSI-X eller timer-tick vekker CPU. Fast commands beholder tight-loop-ytelse.
- Empirisk: 5M budsjett var verre under QEMU/whpx fordi `pause`-VM-exit
  dominerer — på bare-metal inverterer tradeoff. Dokumentert i kildekoden.

### MVFS-på-NVMe (pluggable backend)
- `Backend` enum + `AtomicU8` selector + `use_nvme_backend()` switch i
  `fs/mvfs.rs`. Alle `virtio_blk::*` call-sites routet gjennom dispatcher.
- Boot-order endret: NVMe init før MVFS load; foretrekker NVMe når tilgjengelig.
- Persistens-bevis: `boot_counter = 1` (fersk disk) → full QEMU reboot →
  `boot_counter = 2` fra `[MVFS] loaded 1 entries from disk`.

### Storage throughput baseline (`drivers/storage_bench.rs`)
- TSC-timet 1 MiB sekvensiell write/read + 100 random 512 B reads.
- NVMe: **455 MB/s write, 432 MB/s read, 42 μs random**.
- VirtIO-blk ekskludert — KVM VirtIO status=0xFF quirk kontaminerer tallene
  (retry-workarounden ville blitt målt, ikke enheten).

```
1bbca04  feat: MSI-X + NVMe driver with MVFS-on-NVMe persistence
         10 files changed, 2838 insertions(+), 24 deletions(-)
```

---

## Nøkkeltall (oppdatert 15. april 2026)

| Metrikk | Verdi |
|---------|-------|
| Total commits | 96+ (4 repos) |
| Utviklingsperiode | 23. januar – 15. april 2026 |
| Kernel | Rust no_std, x86-64, Limine bootloader |
| Kernel size | 2400 KB |
| Storage backends | NVMe (primary) + VirtIO-blk (fallback), swappable via MVFS dispatcher |
| NVMe throughput | 455 MB/s write, 432 MB/s read, 42 μs random (1 MiB, 512 B sectors) |
| Interrupts | MSI-X vektor 64 (VirtIO-blk) + 65 (NVMe), legacy IOAPIC behold for keyboard/mouse |
| Syscalls | 100+ (inkl. async TCP 0xE0-E3, W^X 0x32, Draug bridge 0xD0-D1) |
| Modell (on-device) | SmolLM2-135M, Q4_0 kvantisering |
| Modell (Draug) | qwen2.5-coder:7b (L1), gemma4:31b-cloud (L2+) via Ollama |
| WASM host functions | 53 (graphics, network, AI, VFS, system) |
| WASM backend | Dual: wasmi (sandboxed) + silverfir-nano (trusted JIT) |
| JIT self-test | PASS — native x86_64 execution returned 42 |
| Draug skill tree | L1=20, L2=20, L3=20 (100% pass rate) |
| Draug Phase 15 | 4/8 complex tasks COMPLETE (ringbuffer, bump_alloc, bitset, task_queue) |
| TCP shell | port 2222, 12 commands, draug status/pause/resume |
| Anti-gaming | 3 layers: ground-truth injection, tautology detection, mutation testing |
| Security | Source scanner (14 patterns), SSRF protection, W^X enforcement |
| UI freeze | **Zero** — all Draug TCP calls async via EAGAIN |
| Boot persistence | 26-byte state, resumes after restart |
| Synapse DB usage | ~400 KB / 4 MB (10%, bounded) |
| Heap fragmentation | Fixed buffers in hot loop, JitPool for JIT |
| MCP tools | 9+ debugging/konfigurering |
| folkering-proxy | Rust, Chromium + Ollama + WASM compile + security |
| folkering-daq | aarch64 Pi 5, TCP shell, openDAQ streaming |
