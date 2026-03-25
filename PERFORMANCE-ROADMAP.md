# Folkering OS — Performance Roadmap

> Detaljert ytelsesanalyse med konkrete tall, flaskehalser, og
> akselerasjonsstrategier for CPU, GPU, NPU, og multi-core.

---

## Nåværende ytelse (målt)

| Metrikk | QEMU TCG | Bare metal (estimert) |
|---------|----------|----------------------|
| Tokens/sek (prefill) | ~2 | ~50 |
| Tokens/sek (generering) | ~2 | ~50 |
| Latens per token | ~500 ms | ~20 ms |
| GEMM throughput | 273 M elem/s | 768 M elem/s |
| GEMM utnyttelse | 36% av peak | ~80% |
| Attention throughput | 20-50 M elem/s | 20-50 M elem/s |
| CPU-kjerner brukt | 1 av N | 1 av N |

**Hovedflaskehals:** QEMU TCG gir 50-100x overhead. På ekte hardware ville vi sett dramatisk forbedring bare av å fjerne emulatoren.

---

## Flaskehals-rangering

```
┌─────────────────────────────────────────────────────────────────┐
│  #1  SCALAR ATTENTION (22M skalar ops/token)     → 4-8x gain   │
│  #2  SINGLE CORE (ingen parallellisme)           → 2-4x gain   │
│  #3  QEMU TCG OVERHEAD (utenfor kontroll)        → 50-100x     │
│  #4  KV-CACHE f32 (18MB, begrenser kontekst)     → 2x kontekst │
│  #5  F32 GEMM SKALAR (fallback for softmax)      → 2x gain     │
│  #6  CACHE BLOCKING MANGLER (L2 misses)          → 1.5x gain   │
│  #7  FAST_LN() PRESISJON (1-2% feil)             → korrekthet  │
└─────────────────────────────────────────────────────────────────┘
```

---

## Nivå 1: AVX2 Attention (4-8x speedup, lav innsats)

### Problem
Attention Q·K^T dot product er 100% skalar:
```rust
for d in 0..head_dim {
    score += q[q_offset + d] * k_vec[d];  // En multiplikasjon om gangen
}
```

Ved posisjon 128: `30 layers × 9 heads × 128 tokens × 64 dims = 22 millioner` skalare multiplikasjoner.

### Løsning
Bruk eksisterende `simd::dot_f32_avx2()` fra `libtensor/src/simd.rs`:
```rust
// FRA:
let mut score = 0.0f32;
for d in 0..head_dim {
    score += q[q_offset + d] * k_vec[d];
}

// TIL:
let score = simd::dot_f32_avx2(&q[q_offset..q_offset+head_dim], k_vec) * scale;
```

**Estimert gevinst:** 4-8x for attention-delen. Total inference ~2x raskere.

---

## Nivå 2: Multi-Core (2-4x speedup, høy innsats)

### Nåtilstand
- Scheduler støtter kun 1 CPU-kjerne
- Ingen SMP-initialisering (AP-kjerner sovner etter boot)
- Ingen per-CPU task-køer

### Strategi: Per-Layer Parallellisme

```
              ┌── Head 0-2 → Core 0
Layer N ──────┼── Head 3-5 → Core 1
              └── Head 6-8 → Core 2

Synkroniser med barrier mellom layers.
```

### Hva som trengs
1. **SMP Init** (`kernel/src/smp/`):
   - Parse ACPI MADT for AP-prosessorer
   - Send INIT + STARTUP IPI til hver AP
   - Sett opp per-AP GDT, IDT, stack, page tables
   - Estimat: ~500 linjer kernel-kode

2. **Per-CPU Scheduler** (`kernel/src/task/scheduler.rs`):
   - En runqueue per kjerne
   - Work-stealing for lastbalansering
   - Affinitetshint for inference-threads

3. **Parallel Attention** (`libtensor/src/transformer.rs`):
   - Spawn N threads (en per head-gruppe)
   - Bruk arena-partisjonering (hvert head-thread får sin del)
   - Barrier etter attention, før output-projeksjon

**Estimert gevinst:** 2-4x avhengig av antall kjerner.

---

## Nivå 3: KV-Cache Quantization (2x kontekst, medium innsats)

### Nåtilstand
KV-cache lagres som f32: `30 × 9 × 64 × 260 × 4 bytes = 18 MB`

### Strategi: Q8_0 KV-Cache
- Kvantiser K og V til Q8_0 etter de er beregnet
- Dekvantiser on-the-fly under attention
- Halvererer minne: 18 MB → 9 MB
- Tillater 512-tokens kontekstvindu med samme RAM

```rust
// Lagre:
quantize_f32_to_q8_0(&k[..kv_dim], &mut k_q8_buf);
kv_cache.store_q8(k_q8_buf, v_q8_buf);

// Les:
let k_vec = kv_cache.get_key_dequant(t, kv_h, &mut temp_f32);
```

**Presisjonspåvirkning:** < 0.1% per layer (30 × 0.1% = 3% total, akseptabelt).

---

## Nivå 4: Flash Attention (2-4x for lange sekvenser, høy innsats)

### Problem
Naiv attention er O(N²) i minne og beregning:
```
Score = Q × K^T       ← N×N matrise i minne
Attn = softmax(Score) ← Full N×N softmax
Out = Attn × V        ← N×N × N×d
```

For N=256: 256×256 = 65K elementer × 30 layers = 2M midlertidige floats.

### Flash Attention Strategi
Tile attention i blokker som passer i L1 cache (32KB):
```
For each query block Q_i (16 queries):
  For each key block K_j (16 keys):
    S_ij = Q_i × K_j^T         ← 16×16 tile i L1
    m_ij = rowmax(S_ij)
    P_ij = exp(S_ij - m_ij)    ← Online softmax
    O_i += P_ij × V_j          ← Akkumuler output
  End
End
```

**Gevinst:** 2-4x for sekvenser > 128, primært fra bedre cache-lokalitet.

---

## Nivå 5: GPU Compute (10-100x, massiv innsats)

### QEMU VirtIO-GPU
QEMU støtter `-device virtio-gpu-gl` med OpenGL backend. Men:
- Kun 2D rendering (Virgl3D for 3D)
- Ingen compute shader support i standard VirtIO-GPU
- Ville kreve en komplett GPU-driver i kernel

### Realistisk GPU-strategi for Folkering OS

**Alternativ A: VirtIO-GPU med Vulkan Compute**
- QEMU 8.0+ har `virtio-gpu-rutabaga` med Vulkan passthrough
- Krever: PCI VirtIO-GPU driver → Vulkan loader → compute pipeline
- Innsats: 3-6 måneder arbeid
- Gevinst: 10-50x for GEMM

**Alternativ B: VFIO GPU Passthrough**
- Krever dedikert GPU på host
- Full hardware-tilgang via PCI passthrough
- Krever: PCI VFIO driver → GPU-spesifikk driver (NVIDIA/AMD)
- Innsats: 6-12 måneder
- Gevinst: 100x+

**Alternativ C: Shader-basert GEMM via Framebuffer**
- Misbruk QEMU framebuffer som compute target
- Skriv matrise-data til framebuffer, les tilbake resultat
- Ekstremt hacky men mulig for proof-of-concept
- Innsats: 2-4 uker
- Gevinst: 2-5x (begrenset av framebuffer-båndbredde)

### Vurdering
GPU-akselerasjon i QEMU er upraktisk. Den reelle veien til GPU er å kjøre Folkering OS på **ekte hardware** med en diskret GPU.

---

## Nivå 6: NPU / Neural Processing Unit

### Intel NPU (Meteor Lake+)
- Tilgjengelig i nyere Intel-prosessorer
- Akselererer INT8/INT4 matrisemultiplikasjoner
- Krever: MMIO-register driver, firmware-lasting, DMA-kjeder
- QEMU: Ingen emulering tilgjengelig

### Apple Neural Engine
- Ikke relevant (x86-64 OS)

### Qualcomm Hexagon DSP
- Ikke relevant (x86-64 OS)

### Realistisk NPU-strategi
- **Kort sikt:** Bruk AVX-512 VNNI (Vector Neural Network Instructions) på støttede CPU-er. Gir ~2x over AVX2 for int8×int8.
- **Medium sikt:** Intel AMX (Advanced Matrix Extensions) på Sapphire Rapids+. 8×8 tile multiply, ~8x over AVX2.
- **Lang sikt:** Full NPU-driver for Intel Meteor Lake.

```
AVX2:        256-bit   ×  8 lanes  =  8 int32 ops/cycle
AVX-512 VNNI: 512-bit  × 16 lanes  = 64 int8 ops/cycle  (8x)
AMX:          1024-bit × 16 tiles   = 256 int8 ops/cycle (32x)
NPU:          ~10 TOPS dedicated    = hardware inference
```

---

## Nivå 7: Speculative Decoding (2-3x gen speed)

### Konsept
Bruk en liten "draft"-modell (f.eks. SmolLM2-135M selv med Q4_0) til å
foreslå N tokens. Verifiser alle N i én forward pass med hovedmodellen.
Aksepter alle tokens som matcher → 2-3x speedup.

### Krav
- To modeller lastet (eller én modell med to kvantiseringsnivåer)
- Parallell prefill av draft-sekvens
- Token-for-token matching med rejection sampling

### Vurdering
Med kun én 135M-modell er dette ikke direkte anvendelig. Men med
multi-modell support (Nivå 7 i ROADMAP-NEXT-GEN) kan dette gi stor
gevinst.

---

## Nivå 8: Prefill Optimalisering

### Batched Prefill
GEMM støtter allerede M>1 (batch). Under prefill kan vi prosessere
flere tokens i én GEMM-operasjon:

```rust
// FRA: M=1, én token om gangen
gemm_q4_q8(q, q8_buf, wq, 1, dim, n_heads*head_dim, ...);

// TIL: M=total_prompt, alle tokens på en gang
gemm_q4_q8(q_all, q8_buf_all, wq, total_prompt, dim, n_heads*head_dim, ...);
```

**Gevinst:** Bedre cache-utnyttelse, ~2-3x speedup for prefill.
**Krav:** Allokere buffere for hele prompt-sekvensen.

---

## Samlet ytelseskart

```
Nåværende:    ~2 tokens/sek (QEMU TCG)
                │
Nivå 1 (AVX2 attn):     ×2  →  ~4 tok/s
Nivå 2 (Multi-core):    ×3  →  ~12 tok/s
Nivå 3 (KV Q8):         ×1  →  ~12 tok/s (lengre kontekst)
Nivå 4 (Flash Attn):    ×2  →  ~24 tok/s
Nivå 8 (Batch prefill):  ×2  →  ~48 tok/s (prefill only)
                │
Bare metal (fjern QEMU): ×50 →  ~100-200 tok/s
                │
Nivå 5 (GPU):           ×10 →  ~1000-2000 tok/s
Nivå 6 (AMX/NPU):       ×8  →  ~800-1600 tok/s
```

---

## Prioritert handlingsplan

### Sprint 1: Quick Wins (1-2 dager)
- [ ] AVX2 attention dot product (Nivå 1) — **størst enkeltstående gevinst**
- [ ] Fix fast_ln() presisjon (erstatt med 4. ordens polynom)
- [ ] RoPE position tracking ved KV-cache wrap

### Sprint 2: Kontekst & Presisjon (1 uke)
- [ ] KV-Cache Q8_0 kvantisering (Nivå 3)
- [ ] Position Interpolation for lengre kontekst
- [ ] Batched prefill (Nivå 8)

### Sprint 3: Multi-Core (2-3 uker)
- [ ] SMP init (wake AP CPUs)
- [ ] Per-CPU scheduler
- [ ] Parallel attention heads

### Sprint 4: Avansert (1-2 måneder)
- [ ] Flash Attention (Nivå 4)
- [ ] AVX-512 VNNI support
- [ ] Cache-line aligned KV-cache

### Sprint 5: Hardware (3-6 måneder)
- [ ] Real hardware boot (SATA/NVMe driver)
- [ ] GPU compute via Vulkan
- [ ] Intel AMX support

---

## Konklusjon

Den **enkleste og mest impactfulle** forbedringen er **Nivå 1: AVX2 attention**.
Det er en ~20-linjers endring i `transformer.rs` som bruker eksisterende
`simd::dot_f32_avx2()` og gir 4-8x speedup for den mest CPU-intensive
delen av inference.

Den **mest dramatiske** forbedringen er å kjøre på **ekte hardware** (fjerne
QEMU TCG). Dette alene ville gi 50-100x speedup uten en eneste kodeendring.

Multi-core og GPU er store prosjekter, men multi-core er realistisk innen
2-3 uker — APIC/SMP init er veldokumentert, og attention er naturlig
paralleliserbar over heads.
