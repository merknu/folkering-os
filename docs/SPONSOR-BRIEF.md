# Folkering OS — Sponsor Brief

> **The world's first operating system that improves itself while you sleep.**

---

## What Is Folkering OS?

Folkering OS is a bare-metal operating system written entirely in Rust (`no_std`) that boots directly on x86-64 hardware — no Linux, no Windows, no POSIX underneath. It is designed from the ground up as an **AI-native** system where artificial intelligence isn't an app running *on top of* the OS, but is woven into the operating system's DNA.

While conventional operating systems treat AI as a feature you install (Copilot, Siri, Cortana), Folkering OS treats AI as a **fundamental system service** — like memory management or file I/O. The AI agent can inspect system state, generate executable programs on-the-fly, optimize its own code overnight, and communicate with cloud models through a custom TCP-like protocol over a serial port.

---

## Why Does It Matter?

### The Problem
Today's AI assistants live in sandboxes. They can answer questions and generate text, but they can't actually *do* anything with the hardware. ChatGPT can't check your RAM usage. Copilot can't compile and run a program it just wrote. The AI and the OS are strangers sharing the same apartment.

### The Solution
Folkering OS eliminates the boundary. The AI agent:
- **Sees** the entire system state (memory, processes, files, display)
- **Acts** by calling OS tools (list files, check uptime, run commands)
- **Creates** by generating, compiling, and executing WASM programs — from English to pixels in under 60 seconds
- **Learns** by caching programs and evolving them through autonomous overnight optimization
- **Protects** itself through fuzzing ("Nightmare mode") that finds and patches vulnerabilities before they're discovered

### Who Cares?
- **OS researchers** studying the future of human-computer interaction
- **AI/ML engineers** exploring agentic architectures beyond chatbots
- **Embedded systems developers** interested in AI on bare metal
- **Open-source enthusiasts** following a live-built OS from zero to AI-native
- **Investors** in developer tools, AI infrastructure, and novel computing paradigms

---

## Technical Architecture (30-Second Version)

```
┌──────────────────────────────────────────────────────────────┐
│                    Folkering OS                               │
│                                                              │
│  ┌─────────┐  ┌─────────┐  ┌──────────┐  ┌──────────────┐  │
│  │Compositor│  │  Agent  │  │  Draug   │  │ WASM Runtime │  │
│  │  (GPU)   │  │ (ReAct) │  │(Daemon)  │  │  (wasmi)     │  │
│  └────┬─────┘  └────┬────┘  └────┬─────┘  └──────┬───────┘  │
│       │             │            │                │          │
│  ┌────┴─────────────┴────────────┴────────────────┴───────┐  │
│  │              MCP Layer 4 Transport                      │  │
│  │     COBS framing | CRC-16 | Session IDs | ACK/NACK     │  │
│  │     Retransmission Queue | WASM Chunking                │  │
│  └─────────────────────┬──────────────────────────────────┘  │
│                        │ COM2 Serial (115200 baud)          │
│  ┌─────────────────────┴──────────────────────────────────┐  │
│  │         Rust no_std Microkernel (x86-64)                │  │
│  │  Preemptive SMP | VirtIO-GPU | VirtIO-Blk | IPC        │  │
│  │  PS/2 Input | APIC Timer | Physical Memory Manager      │  │
│  └─────────────────────────────────────────────────────────┘  │
└──────────────────────────────────────────────────────────────┘
                         │
                    COM2 TCP Socket
                         │
┌──────────────────────────────────────────────────────────────┐
│              Python LLM Proxy (Host Machine)                 │
│                                                              │
│  ┌────────────────────────────────────────────────────────┐  │
│  │           4-Tier Hybrid Model Router                    │  │
│  │                                                        │  │
│  │  FAST ──► Qwen 7B (local Ollama, free, instant)       │  │
│  │  MEDIUM ► Gemini 3.1 Flash Lite (cloud, cheap)        │  │
│  │  HEAVY ─► Gemini 3 Flash (cloud, moderate)            │  │
│  │  ULTRA ─► Gemini 3.1 Pro (cloud, rate-limited)        │  │
│  │                                                        │  │
│  │  Auto-escalation: FAST → MEDIUM → HEAVY → ULTRA       │  │
│  └────────────────────────────────────────────────────────┘  │
│                                                              │
│  ┌────────────┐  ┌────────────┐  ┌─────────────────────┐   │
│  │  Context   │  │   WASM     │  │  Retransmission     │   │
│  │  Manager   │  │   Cache    │  │  Queue              │   │
│  │  (3-tier)  │  │  (source   │  │  (ACK/NACK/retry)   │   │
│  │            │  │   + binary)│  │                     │   │
│  └────────────┘  └────────────┘  └─────────────────────┘   │
└──────────────────────────────────────────────────────────────┘
```

---

## The Five Breakthroughs

### 1. MCP Layer 4 — A Custom Network Protocol Over Serial

The OS communicates with the host-side LLM proxy through a virtual serial port. But raw serial is unreliable — bytes get lost, frames corrupt, zombie processes steal data. So we built a **TCP-like transport protocol from scratch**:

- **COBS framing** — byte-stuffing that guarantees packet boundaries on raw serial
- **CRC-16** — integrity check on every frame (detects corruption)
- **Postcard serialization** — zero-allocation binary encoding (Rust `no_std` compatible)
- **Session IDs** — random u32 per boot, rejects data from dead proxy instances
- **Sequence numbers + ACK/NACK** — guaranteed delivery with correlation
- **Retransmission queue** — 3 automatic retries on timeout, auto-escalation
- **WASM chunking** — large binaries split into 3KB frames for reliable transfer

This is the same reliability model as TCP/IP, but running over a 115200-baud serial port inside a virtual machine.

### 2. ReAct Agent Loop — AI That Uses Tools

The AI doesn't just answer questions. It operates a **ReAct (Reason + Act) loop**:

```
User: "What's the system status?"
Agent: {"tool": "system_info", "args": ""}        → calls OS tool
OS:    "Uptime: 3600s, Memory: 45/2048MB (2%)"    → returns result
Agent: {"tool": "list_tasks", "args": ""}          → calls another tool
OS:    "5 tasks running"                           → returns result
Agent: {"answer": "System healthy: 1hr uptime, 2% RAM, 5 tasks"}
```

The agent can chain up to 10 tool calls per session, with a 120-second timeout and circuit breaker. All communication flows through the MCP protocol.

### 3. WASM JIT Toolsmithing — From English to Pixels

A user types "draw a bouncing ball" into the omnibar. What happens:

1. The compositor sends the request to the LLM proxy via MCP
2. Gemini Flash Lite generates Rust code for a WASM app
3. The proxy auto-fixes common LLM mistakes (missing `unsafe`, wrong extern declarations)
4. Cargo compiles to a 400-byte WASM binary
5. The binary is chunked over MCP back to the OS
6. The OS executes it in a sandboxed WASM runtime (wasmi, fuel-metered)
7. The bouncing ball appears on the VirtIO-GPU display

**Time from English to pixels: ~60 seconds.** The generated app is cached — next time the user asks for "bouncing ball", it loads in 0.01 seconds.

Features: `--force` (skip cache), `--tweak "make it red"` (modify existing app).

### 4. Draug Daemon — The Sleepless Watcher

Named after the Norse undead that never sleeps, **Draug** is a background AI daemon that monitors the system:

- **Ticks** every 10 seconds, collecting RAM usage and idle state
- **Analyzes** after 30 seconds of user idle — sends observations to LLM for assessment
- **Predicts** what the user will ask next (command history frequency analysis)
- **Yields** to user-facing tasks (Token Scheduler — AI attention follows user intent)

Draug is the foundation for AutoDream.

### 5. AutoDream — Software That Evolves Overnight

This is the headline feature. When the system is idle for 15 minutes, Draug enters **dream mode** and begins autonomously optimizing cached programs. Three dream modes rotate:

| Mode | Goal | Evaluation | Acceptance |
|------|------|------------|------------|
| **Refactor** | Fewer CPU cycles | RDTSC benchmark: V1 vs V2 × 10 iterations | V2 must be faster |
| **Creative** | Better visuals | Headless render → text description to LLM | Compiles = accepted |
| **Nightmare** | Crash resistance | Fuzz with extreme inputs (w=0, h=0, t=MAX) | Survives = accepted |

**Three Strikes Rule:** If an app fails to improve in 3 consecutive Refactor dreams, it's marked "perfected" and Draug stops trying. Creative and Nightmare modes can still run.

**Daily Budget:** Maximum 10 cloud API calls per day for dreams (persistent `dream_budget.json`).

**The result:** You go to sleep. In the morning, your calculator app is 15% faster. Your clock display has smoother colors. Your bouncing ball no longer crashes when the screen is zero-sized. **The OS literally improved itself while you slept.**

---

## By The Numbers

| Metric | Value |
|--------|-------|
| Language | Rust (nightly, `#![no_std]`) |
| Kernel size | ~60 KB |
| Boot time | ~5 seconds (WHPX) |
| RAM usage | 2% of 2GB |
| CPU cores | 4 (SMP with parallel GEMM) |
| WASM host functions | 17 (drawing, input, time, surface, files) |
| MCP protocol | Layer 4 (COBS + CRC-16 + Postcard + ACK) |
| AI tiers | 4 (local Qwen → Gemini Lite → Flash → Pro) |
| Compositor frame time | <1ms (shadow buffer + targeted damage) |
| WASM gen time | ~60s (Gemini) / ~90s (Qwen local) |
| AutoDream budget | 10 dreams/day, 5/session, 10min cooldown |
| Development time | 2 months (Jan–Apr 2026) |
| Lines of Rust | ~15,000 (kernel + userspace) |
| Lines of Python | ~1,500 (proxy + tools) |

---

## What Makes It Unique (Competitive Moat)

1. **No other bare-metal OS has an agentic AI loop.** Redox, SerenityOS, and Theseus have no AI integration. Folkering OS was designed AI-first.

2. **No other system generates and executes code on-the-fly from natural language on bare metal.** Cloud IDEs can do this, but they require an entire browser + OS stack underneath.

3. **No other system evolves its own software autonomously.** AutoDream is, to our knowledge, the first implementation of autonomous program optimization in a production-oriented OS.

4. **The MCP transport layer is custom-built.** While Model Context Protocol exists as a specification, Folkering's implementation is the only one running over raw serial with COBS framing, session multiplexing, and retransmission — in a `no_std` environment.

5. **The entire stack is one person's work** (with AI assistance). This demonstrates that the barrier to building novel operating systems has fundamentally changed with AI-augmented development.

---

## The Vision

Folkering OS is not trying to replace Windows or Linux. It's a **research prototype** that asks:

> *What if the operating system was designed for AI from day one?*

Today's AI assistants are guests in someone else's house. Folkering OS builds the house around the AI. The filesystem is semantic (vector search, not folders). The UI is ephemeral (generated on demand, not installed). The scheduler prioritizes AI token budgets, not just CPU time. And the software improves itself through dreaming.

This is what computing looks like when you stop treating AI as an afterthought.

---

## Links

- **GitHub:** [github.com/merknu/folkering-os](https://github.com/merknu/folkering-os)
- **Branch:** `ai-native-os`
- **Company:** Meray Solutions AS — [meray.no](https://meray.no)
- **Developer:** Knut Ingmar Merødningen
- **Contact:** kontakt@meray.no

---

*Built in Nesbyen, Norway. Powered by Rust, Gemini, and sleepless nights.*
