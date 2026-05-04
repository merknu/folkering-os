# Phase D — Hybrid AI-native Inference Inside Folkering OS

**Status:** D.1 (hybrid router + IPC abstraction) landed 2026-05-04 in
PR (TBD — this commit). D.2 onward unimplemented. This doc is the
Phase D bible.

**Strategy update (2026-05-04):** the goal is *hybrid* inference, not
a hard cut from the network. Local Burn engine becomes the primary
when it's ready; the proxy stays as a transparent fallback for
heavy-lift requests. The router in `userspace/inference/` is the seam
that lets us land local capability one model at a time without
breaking any caller.

**Goal:** Run an LLM (Qwen2.5, Gemma, Phi, etc.) *inside* Folkering OS — no
Ollama on the host, no folkering-proxy bridge, no LAN dependency. The OS
should be able to think while disconnected.

---

## 1. The current arrangement (what we're moving away from)

Today, Draug's "thinking" is a TCP round-trip to a host that runs Ollama:

```
folkering-os (VM)  →  TCP 14711  →  folkering-proxy  →  HTTP 11434  →  Ollama  →  Qwen2.5-coder:7b
                                                                     ↓
                                                                cargo test
                                                                     ↓
                                                              verdict back
```

This *works* — Phase 17 closed the autonomous-refactor loop end-to-end on
real KVM hardware (see project memory `folkering-phase17-proxmox-live.md`).
But every L1 task burns a few seconds of network + a host with a beefy GPU.
The OS isn't really thinking; it's outsourcing thinking and orchestrating
the result.

For Phase D the inference engine moves *into* the guest. The proxy stays
useful for `cargo test` validation and `FETCH_SOURCE`, but the LLM is local.

---

## 2. Three runtime options on the table

### 2a. **Candle / Burn (native Rust)** — the obvious default

| | Candle | Burn |
|---|---|---|
| no_std capable | partial (with feature flags) | yes (`burn-no-std`) |
| Backends | CPU (matmul kernels), CUDA, Metal | CPU, WGPU, Candle, NDArray, LibTorch |
| Tensor format | proprietary GGUF-ish loader | pluggable |
| Maturity | high (HuggingFace product) | medium (active dev, fewer model recipes) |
| WASM target | yes | yes (WGPU backend) |
| Folkering fit | good — already aligned with our Rust-everywhere ethos | excellent — backend abstraction maps cleanly to virtio-gpu compute |

**Take.** Burn is the more "Folkering-shaped" choice. It already separates
the math kernels from the runtime, which is exactly the seam we want when
the math kernels graduate from CPU to virtio-gpu compute (option 2c below).
Candle has more pre-baked model recipes but ties them to its CUDA/Metal
backends, neither of which we have.

The existing `userspace/inference-server` crate (currently skipped at boot
to save 400 MB of RAM, see `kernel/src/lib.rs:574`) was a Burn target. It
needs a refresh against the current Burn API but it's the right hat.

### 2b. **LiteRT-LM (Google's TFLite-LM, C++ → WASM)** — *not* the engine

LiteRT-LM is the new name for TensorFlow Lite, with a focused LLM runner
on top. Their pitch is "C++ runtime that compiles to WASM, runs on every
device including phones."

The temptation is to compile their WASM target with our wasmi 2.0 runtime
and run any HuggingFace model. Don't.

**Why not as the engine:**
- C++→WASM has roughly **2-3× perf overhead** vs. native Rust math kernels.
  We'd be paying that on every matmul.
- Their main asset is the **model loader** (turns HuggingFace weights into
  a runnable graph), not the runtime. The runtime is fine but isn't a moat.
- WASM-on-wasmi means another VM layer between guest userspace and the
  metal. The math hot path becomes wasmi → C++/WASM → math. Rough.
- Pulls in a vendored TensorFlow Lite codebase (millions of lines). Audit
  surface and licensing review for AGPL-cohabitation are non-trivial.

**Where it does fit:** as an **interop layer**, not the engine. Use it
once, on the host or at build time, to convert HuggingFace weights into a
format our Burn runtime can consume. Keep the runtime native.

### 2c. **VirtIO-GPU compute shaders** — the perf ceiling

The `VIRTIO_GPU_F_VIRGL` feature bit is already detected at init (see
`kernel/src/drivers/virtio_gpu/mod.rs:117`). Today we only do 2D scanout;
VirGL would let us submit GLSL/SPIR-V compute kernels that run on the
host's GPU.

**Why this is the long game:**
- Matmul on a modern dGPU is **50-100× faster** than CPU matmul.
- Quantization (Q4, Q5, Q8) maps cleanly to integer compute kernels —
  doesn't need full FP32 stack.
- The kernel work isn't unique to inference; once VirGL is wired,
  WebGPU-style compute is available to *anything* (the WASM apps, Draug's
  code generation, future compositor effects).

**Why not first:**
- VirGL is a months-of-work bring-up (cross-compile shaders, pipeline
  state, synchronization, command submission). Way beyond a single PR.
- Tied to host GPU presence + virtio-gpu support for VirGL (Proxmox
  default exposes VirGL but blob requires udmabuf, see project memory
  `folkering-virtio-gpu-blob-host-reqs.md` — same family of host-side
  knobs).
- The tooling (kernel-side compute compiler, tensor↔shader translation)
  doesn't exist yet.

The right time to start VirGL is *after* a CPU-only inference path is
shipping and the bottleneck is empirically the matmul, not anything else.

---

## 3. Phasing (revised 2026-05-04)

```
Phase D.1 — Inference router + IPC abstraction  ← LANDED 2026-05-04
   • New `userspace/inference/` crate. IPC service that owns:
       - `router::dispatch`         — picks local vs proxy
       - `local_backend::run`       — D.1 stub: NotImplemented
       - `proxy_backend::run`       — calls libfolk::sys::llm_generate
       - `tensor_math::self_test`   — runs at boot (2x2 matmul)
   • Wire format: shmem region with `InferenceWire` header carrying
     model name + prompt + result buffer. Same shape as
     `llm_generate` so the proxy backend is a 5-line delegator.
   • Burn 0.21 verified to compile in our `no_std + alloc` custom
     target (`burn-tensor` with `default-features = false`).
   • Out-of-tree: callers (draug-daemon) still call `llm_generate`
     direct; D.1's only deliverable is the router skeleton + proof
     Burn fits. Migrating callers is D.1.5.

Phase D.2 — Burn local backend, dummy matmul over IPC
   • Implement Burn's `Backend` trait for a custom CPU backend
     (`FolkeringCpu`) that owns `Vec<f32>` storage and uses
     `tensor_math::matmul` under the hood.
   • Local backend's `run` recognizes a sentinel model name like
     `"local:matmul-test"` and runs a 32×32 matmul over IPC,
     returning the L2 norm of the result as a sanity-check string.
   • Caller (a tiny test app) verifies routing is functional:
     same IPC request reaches the local backend instead of the
     proxy.

Phase D.3 — Real model: Qwen2.5-0.5B Q4 forward pass
   • Quantized weights in Synapse VFS (loaded via libfolk::sys::synapse).
   • Tokenizer: re-use the BPE merger from the legacy
     inference-server crate (still in tree). Project memory:
     `folkering-bpe-tokenizer.md`.
   • Forward pass via Burn tensors backed by `FolkeringCpu`. Per-row
     yield_cpu so the GUI stays responsive.
   • Success metric: end-to-end "Hello → " → token via local backend.
     Latency target: ≤2s per token (Q4 0.5B on CPU is realistic).

Phase D.4 — KV-cache + memory plumbing
   • Pre-allocated KV-cache buffer in the inference task's heap;
     bump-allocator-friendly (alloc once, reuse forever per session).
   • Streaming response: extend the wire to support a "session"
     handle; subsequent prompts append to the same KV without re-
     processing the prefix.
   • At this point draug-daemon's TCP/Ollama path can be retired —
     the local backend handles its routine workload, proxy stays
     for `cargo test` + heavy models only.

Phase D.5 — VirGL compute bring-up
   • Negotiate VIRTIO_GPU_F_VIRGL during init (already detected).
   • Submit a SPIR-V compute kernel (vector add). Verify result
     via VIRTIO_GPU_RESOURCE_READBACK.
   • Translate one Burn matmul kernel to GLSL compute. Plumb the
     `FolkeringGpu` backend, swap router's local backend over.
   • Success metric: 8B model at >20 tokens/sec on a host with
     a 2080Ti-class GPU.
```

The milestones are stackable. **D.1 unlocks the seam without changing
behavior**; D.2 plants the local engine alongside the proxy fallback;
D.3 makes the local engine actually answer; D.4 makes it practical;
D.5 makes it competitive.

---

## 4. Constraints we have to remember

- **Memory.** A 7B Q4 model is ~4 GB. Folkering's PMM allocates from a
  2 GB total guest RAM today (Proxmox VM 800 default). Realistic local
  models for Phase D.1 are 0.5B–3B. Anything bigger waits for D.4 GPU
  offload OR a memory-budget bump.
- **Userspace target triple.** `userspace/target/x86_64-folkering-userspace`
  is a custom no_std target. Burn needs careful feature flagging to
  avoid pulling in std-required deps. The existing inference-server
  crate already navigated this once; the recipe is in its Cargo.toml.
- **Bump allocator.** Folkering apps run on a per-task bump allocator
  with no general-purpose dealloc (see `userspace/folkui-demo/src/main.rs`
  for the pattern). Inference workloads allocate KV-cache up front and
  reuse — that's a natural fit for bump allocators, but tensor-graph
  rebuilders that free intermediate tensors will leak. Burn's static
  graph mode handles this correctly; the dynamic graph mode does not.
- **Capability model.** Inference must be a per-task capability so the
  scheduler can prevent a runaway WASM app from monopolizing the
  inference engine. The existing capability table (`grant_inference`,
  similar shape to `grant_framebuffer`) is the seam.

---

## 5. What this means for current work

- **Don't rip out folkering-proxy.** It's the test harness for Phase D
  itself: we'll compare local-Burn output against proxy-Ollama output
  during validation. Keep both wires hot.
- **Don't pre-build VirGL kernels yet.** Premature; we don't know which
  matmul shape is the bottleneck without running CPU-only first.
- **Do tighten the inference syscall surface.** The current `0x70
  ASK_GEMINI` path was wired for the cloud round-trip. Phase D.1's
  Burn engine will need a richer interface — streaming token output,
  KV-cache lifecycle. Plan for that when we touch the syscall.
- **Do keep an eye on Burn 0.18+ release notes.** Their no_std story is
  improving; the day it lands a clean WASM backend with INT4 support is
  the day Phase D.3 becomes a few-week sprint instead of a year-long
  research project.

---

## Open questions (parked, not answered)

- **Tokenizer.** Most ship as Python (HuggingFace tokenizers crate has a
  Rust port — `tokenizers` crate, but it's std-only). Either re-implement
  the BPE merger we already have (project memory: `folkering-bpe-tokenizer.md`)
  per-model, or compile a single tokenizer to WASM and ship in initrd.
- **Sampling.** Greedy is trivial; nucleus / temperature is a few lines.
  But proper repetition-penalty and structured-output need state we
  don't yet plumb. Belongs in inference-server, not the kernel.
- **Multi-tenant inference.** If two apps both want to ask Draug
  something, do they share one model instance with serialized requests,
  or load two? Default: serialize. Revisit when we have apps that
  actually compete.
- **Model storage.** A 500 MB model in initrd is a 500 MB initrd. That
  hurts boot time and image size. Synapse VFS as the model store with
  on-demand loading from a separate disk image is probably the answer.
