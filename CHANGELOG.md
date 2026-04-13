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

### Silverfir-nano JIT (branch: silverfir-nano-wasm-hybrid)
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

### Commits
```
ai-native-os branch (12 commits):
  29adabf  Phase 13-16 + TCP remote shell
  9db1ea1  Ping crash fix, skip KHunt, bridge after restore
  e76a97e  Cached proxy ping, adaptive interval, net/df
  aeb6a2a  Shell improvements — draug current, clear, remove traceroute
  d90f99e  Audit: try_lock, ping cache, bridge rate-limit
  c4596f4  Audit: iter count, L1 persistence, abandoned task guard
  4933684  Heap fragmentation fixes
  bdd8457  Atomic memory ordering (Acquire/Release)

silverfir-nano-wasm-hybrid branch (8 commits):
  73cd49b  WasmBackend dual-runtime enum
  b51c1ea  JIT scaffold + CodeBuffer
  e21de43  W^X memory + protect_in_table
  57019be  WASM parser + x86_64 translator
  48d16c1  Unsigned comparisons + load/store
  f5bac38  JIT self-test PASS (compilation)
  933e18b  Native execution — returned 42
  800037c  All TCP timeouts reduced to 120K

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
