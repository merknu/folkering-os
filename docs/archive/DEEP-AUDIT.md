# Folkering OS — Deep Technical Audit

> En dyp analyse av subtile bugs, arkitektoniske begrensninger, og
> uoppdagede feil. Basert på full kodegjennomgang av kernel, IPC,
> transformer, og minnehåndtering.

---

## DEL 1: KERNEL-BUGS (Tikkende bomber)

### BUG 1: Deadlock i IPC Reply (KRITISK)
**Fil:** `kernel/src/ipc/receive.rs:167-206`

```rust
let sender_lock = sender_task.lock();        // Lås 1
let current_id = current.lock().id;          // Lås 2 mens lås 1 holdes
```

Samme funksjon re-låser begge objektene i forskjellig rekkefølge lenger ned. Hvis to tasks gjør IPC samtidig, oppstår sirkulær ventetilstand.

**Risiko:** Hele OS fryser under tung IPC-last (inference + compositor + shell).
**Samme bug:** `ipc_reply_with_token()` (linje 324-372).

---

### BUG 2: Manglende interrupt-disable i IPC Send
**Fil:** `kernel/src/ipc/send.rs:117-144`

```rust
current_lock.state = TaskState::BlockedOnSend(target);  // Steg 7
// Timer-interrupt kan fyre HER og se inkonsistent tilstand
crate::task::scheduler::yield_cpu();                     // Steg 9
```

Mellom steg 7 og 9 kan preemption-handleren se at tasken er `BlockedOnSend` men den kjører fortsatt.

---

### BUG 3: Page Table Memory Leak
**Fil:** `kernel/src/memory/paging.rs:438-458`

```rust
pub fn free_task_page_table(pml4_phys: u64) -> Result<(), MapError> {
    // TODO: Walk and free intermediate page tables for user space
    physical::free_page(pml4_phys as usize);  // Kun PML4 frigjøres!
    Ok(())
}
```

Alle mellomliggende sidetabeller (PDPT, PD, PT) lekker. Hver task-spawn lekker ~16KB i sidetabeller som aldri frigjøres.

---

### BUG 4: Bruker-stack uten guard page
**Fil:** `kernel/src/task/spawn.rs:178-203`

Kernel-stacken har guard page, men bruker-stacken har INGEN. En buffer underflow i userspace krasjer hele OS i stedet for å trigge en håndterbar `#PF`.

---

### BUG 5: Page fault handler henger
**Fil:** `kernel/src/arch/x86_64/idt.rs:42-58`

```rust
// Page fault handler: just loops forever
loop { x86_64::instructions::hlt(); }
```

Ingen demand paging, ingen copy-on-write, ingen recovery. Enhver uventet page fault = frys.

---

## DEL 2: INFERENCE ENGINE — Numeriske feller

### FEIL 1: RoPE Position Mismatch etter KV-Cache Wrap (HØY)
**Filer:** `libtensor/src/transformer.rs:205-206`, `libtensor/src/kv_cache.rs:145`

KV-cachen er en ring buffer med 256 plasser. Når posisjon > 256:
- **Query** ved pos 300 → RoPE rotasjon for posisjon 300
- **Key** lagret ved fysisk plass `300 % 256 = 44` → har RoPE for posisjon 44

Attention beregner `Q·K^T` mellom inkompatible posisjoner. Resultatet: **semantisk drift etter 256 tokens med en hard klippe**.

StreamingLLM-tilnærmingen (4 sink tokens + ring) er riktig i prinsippet, men posisjonsinformasjonen følger IKKE med wrappingen.

---

### FEIL 2: fast_ln() har 1-2% feil (MEDIUM)
**Fil:** `libtensor/src/ops.rs:350-354`

```rust
fn fast_ln(x: f32) -> f32 {
    let bits = x.to_bits() as f32;
    bits * 8.2629582e-8 - 87.989971
}
```

Lineær bit-hack med 1-2% relativ feil. Brukes i RoPE base-frekvens beregning (`1.0 / fast_exp(exponent * ln_base)`). Påvirker posisjonskoding-nøyaktighet.

**Alle andre approx er < 0.01% feil:**
| Funksjon | Metode | Feil |
|----------|--------|------|
| fast_rsqrt | 2. ordens Newton | ~0.01% |
| fast_exp | 6. ordens polynom | ~0.003% |
| fast_sin | 5. ordens Chebyshev | ~0.01% |
| **fast_ln** | **Lineær bit-hack** | **1-2%** ← svakeste ledd |

---

### FEIL 3: Attention er 100% skalar (YTELSE)
**Fil:** `libtensor/src/transformer.rs:222-250`

GEMM har AVX2-optimalisering, men attention-beregningen (Q·K^T + softmax + V-vekting) er ren skalar:

```rust
for t in 0..seq_len {
    let k_vec = kv_cache.layer(layer).get_key(t, kv_h);
    let mut score = 0.0f32;
    for d in 0..head_dim {
        score += q[q_offset + d] * k_vec[d];  // Skalar dot product!
    }
    att[t] = score * scale;
}
```

Ved posisjon 128: `30 layers × 9 heads × 128 × 64 = 22M` skalare multiplikasjoner per token.
AVX2 ville gi **8x speedup** for dette.

---

### FEIL 4: GQA ikke utnyttet
**Fil:** `libtensor/src/transformer.rs:213`

SmolLM2 bruker Grouped Query Attention (3 KV-heads for 9 Q-heads). Koden beregner `kv_group_size` men gjør fortsatt full attention for alle 9 heads uten å dele K/V-beregninger.

---

### FEIL 5: Nucleus Sampling er O(V×K) (MINOR)
**Fil:** `inference-server/src/main.rs:1057-1081`

Lineær søk gjennom 49,152 vocab-entries × 128 nucleus-slots = ~6.3M sammenligninger per sample-steg. En min-heap ville redusere til O(V × log K).

---

## DEL 3: Hva som mangler for Next-Gen

### Mangler: Flash Attention
**Impact:** Høy. Flash Attention tiler attention-beregningen for L2 cache-lokalitet. Ville gi 2-4x speedup for lange sekvenser.

### Mangler: KV-Cache Quantization
**Impact:** Høy. KV-cache lagres som f32 (17MB for 256 tokens × 30 layers). Q8_0 KV-cache ville halvere til 8.5MB og tillate dobbelt så langt kontekstvindu.

### Mangler: Speculative Decoding
**Impact:** Medium. Bruk en liten "draft"-modell for å foreslå N tokens, verifiser med hovedmodellen i én forward pass. Gir 2-3x speedup for generering.

### Mangler: Batched Inference
**Impact:** Medium. GEMM støtter allerede M>1 (linje 39 i gemm.rs), men brukes alltid med M=1. Batching ville tillate parallell prefill av flere prompts.

### Mangler: Paged Attention (vLLM-stil)
**Impact:** Høy for produksjon. Dynamisk allokering av KV-cache blokker eliminerer fragmentering og tillater mye lengre kontekstvinduer.

### Mangler: Position Interpolation / ALiBi
**Impact:** Høy. Uten dette er kontekstvinduet hardlåst til 256 tokens. PI eller ALiBi ville tillate skalering til 2048+ uten retraining.

---

## DEL 4: Scheduler & Concurrency

### Prioritetsinversjon
**Fil:** `kernel/src/task/scheduler.rs:45-146`

Scheduleren bruker en global `spin::Mutex` for task-tabellen. En lav-prioritets task (synapse) som holder denne mutex-en blokkerer compositor (høy prioritet) fra å sjekke task-tilstander.

### Ingen deadline-basert preemption
Timer-preemption bruker en tick-teller som kan wrappe hvis interrupts er deaktivert for lenge.

### Livelock-risiko
Hvis alle tasks er Runnable og spinner på yield, når scheduleren aldri `hlt()`.

---

## DEL 5: Prioritert fiks-liste

### MUST FIX (krasj-forebyggende)
| # | Bug | Alvorlighet | Innsats |
|---|-----|-------------|---------|
| K1 | IPC deadlock (lås-rekkefølge) | KRITISK | Medium |
| K2 | Manglende interrupt-disable i send | HØY | Lav |
| K3 | Page table memory leak | HØY | Medium |
| K4 | Task kill/exit/restart | KRITISK | Høy |

### SHOULD FIX (ytelse/korrekthet)
| # | Issue | Impact | Innsats |
|---|-------|--------|---------|
| I1 | RoPE position mismatch ved wrap | Semantisk drift | Medium |
| I2 | Attention AVX2-optimalisering | 4-8x speedup | Medium |
| I3 | fast_ln() presisjon | Posisjonskoding-drift | Lav |
| I4 | Filskriving (Synapse write) | Persistens | Høy |

### NICE TO HAVE (neste-gen features)
| # | Feature | Impact | Innsats |
|---|---------|--------|---------|
| N1 | Flash Attention | 2-4x speedup | Høy |
| N2 | KV-Cache Q8_0 | 2x kontekstvindu | Medium |
| N3 | Position Interpolation | 8x kontekstvindu | Medium |
| N4 | HTTP inference server | Ekstern tilgang | Medium |
| N5 | Speculative decoding | 2-3x gen speed | Høy |

---

## Konklusjon

Folkering OS har tre kategorier av teknisk gjeld:

1. **Kernel-stabilitet:** IPC deadlocks og memory leaks er tikkende bomber. Under normal bruk treffer du kanskje ikke disse, men under last (lang inference + UI + nettverkskall) kan de trigges.

2. **Inference-presisjon:** RoPE-feilen ved KV-cache wrap er den viktigste. Den begrenser reelt kontekstvindu til ~200 tokens (4 sink + 196 ring) før semantisk drift. `fast_ln()` er en sleeper — 1-2% feil som akkumulerer gjennom 30 layers.

3. **Ytelsestak:** Skalar attention er flaskehalsen. AVX2-optimalisering av attention dot products ville gi den største enkeltstående speedup (~4x). Flash Attention ville doble dette igjen.

Det mest impactfulle enkelt-steget? **K1 + I2**: fiks IPC deadlocks og legg til AVX2 attention. Gir stabilitet + synlig performance-forbedring.
