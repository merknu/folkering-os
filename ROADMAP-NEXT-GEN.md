# Folkering OS — Roadmap to Next-Gen AI OS

> Etter en grundig systemrevisjon av hele kodebasen, er dette en ærlig
> vurdering av hva som mangler for å gjøre Folkering OS til et ekte
> neste-generasjons AI-operativsystem.

---

## Nåværende tilstand: Hva vi HAR

```
✅ Bare-metal Rust x86-64 microkernel
✅ Preemptive multitasking (6 userspace tasks)
✅ IPC message passing (635 round-trips/15s)
✅ Grafisk compositor med vindushåndtering
✅ SmolLM2-135M on-device inference (Q4_0)
✅ BPE tokenizer med 98.7% parity
✅ Async token streaming via TokenRing
✅ Visual Inspection Studio (attention heatmaps)
✅ VirtIO Control Sector (zero-recompile config)
✅ Activation Monitor (MSE health telemetry)
✅ Network: ICMP, DNS, TLS 1.3, GitHub API
✅ SQLite-basert VFS (lesing)
```

---

## Hva som MANGLER: Prioritert etter alvorlighetsgrad

### TIER 1 — Kritisk: OS-stabilitet (uten dette krasjer vi)

#### 1.1 Task Kill / Exit / Restart
**Status:** IKKE IMPLEMENTERT
**Problem:** Når en prosess krasjer (f.eks. inference-server), kan den ikke stoppes eller startes på nytt. Døde tasks forblir i TASK_TABLE for alltid. 8KB kernel-stack lekker per krasj.
**Konsekvens:** En eneste krasj i inference = reboot nødvendig.
**Filer:** `kernel/src/task/task.rs` (mangler `kill_task`, `exit_task`)

#### 1.2 Watchdog / Syscall Timeout
**Status:** IKKE IMPLEMENTERT
**Problem:** Hvis en task henger i en syscall (f.eks. venter på IPC fra en død task), blokkerer den for alltid. Ingen timeout-mekanisme.
**Konsekvens:** Cascading hangs — én død task tar ned hele systemet.
**Filer:** `kernel/src/ipc/send.rs`, `kernel/src/ipc/receive.rs` (mangler `ipc_receive_timeout`)

#### 1.3 Heap Exhaustion Recovery
**Status:** FATAL PANIC
**Problem:** 16MB kernel-heap delt mellom alle subsystemer. Ved utmattelse: `panic!("Kernel heap exhausted")`. Ingen graceful degradation.
**Konsekvens:** Memorylekkasje = full systemkrasj.
**Filer:** `kernel/src/memory/heap.rs:59`

#### 1.4 Buddy Allocator Disabled
**Status:** KOMMENTERT UT
**Problem:** Fysisk minneallokator (buddy) er delvis deaktivert. Kun single-page allokeringer fungerer.
**Konsekvens:** Kan ikke allokere sammenhengende fysisk minne > 4KB.
**Filer:** `kernel/src/memory/physical.rs:56-59`

---

### TIER 2 — Viktig: Funksjonalitet (uten dette er OS-et halvferdig)

#### 2.1 Filskriving
**Status:** IKKE IMPLEMENTERT
**Problem:** VFS (Synapse) er read-only. Kan ikke lagre inference-resultater, brukerdata, eller konfigurasjon. Ingen `SYS_CREATE`, `SYS_DELETE`, `SYS_WRITE` syscalls.
**Konsekvens:** Alt arbeid forsvinner ved reboot.
**Filer:** `userspace/synapse/src/main.rs`, `userspace/libsqlite/src/`

#### 2.2 Multi-modell Support
**Status:** HARDKODET
**Problem:** `model.gguf` er bakt inn i boot-imaget. Kan ikke bytte modell uten å rekompilere.
**Konsekvens:** Låst til SmolLM2-135M Q4_0 for alltid.
**Filer:** `userspace/inference-server/src/main.rs:1620-1640`

#### 2.3 HTTP Server for Inference API
**Status:** IKKE IMPLEMENTERT
**Problem:** Kun HTTP klient (kan hente fra GitHub). Ingen server. Kan ikke eksponere inference som en API for eksterne klienter.
**Konsekvens:** AI er kun tilgjengelig via OS-ets egen GUI.
**Filer:** `kernel/src/net/tls.rs` (kun klient-modus)

#### 2.4 Clean Shutdown / ACPI
**Status:** TODO-KOMMENTARER
**Problem:** Ingen `SYS_SHUTDOWN`. ACPI er merket "TODO". Ingen måte å synce filer eller stoppe tasks gracefully.
**Konsekvens:** Urent avslutning → potensielt tap av SQLite-data.
**Filer:** `kernel/src/arch/x86_64/acpi.rs:13-14`

---

### TIER 3 — Moderat: Brukeropplevelse

#### 3.1 Clipboard & Text Selection
**Status:** STUBS
**Problem:** Ctrl+C/V sender scancodes men gjør ingenting. Kan ikke kopiere tekst fra AI-output.
**Filer:** `kernel/src/drivers/keyboard.rs:299-305`

#### 3.2 Scrollback & History
**Status:** 32-LINJERS BUFFER
**Problem:** Terminal beholder kun siste 32 linjer. Scrolling er ikke implementert. Kommandohistorikk (pil opp) mangler.
**Filer:** `userspace/compositor/src/window_manager.rs:44`

#### 3.3 Tab Completion
**Status:** IKKE IMPLEMENTERT
**Problem:** Ingen autocompletjon i shell. Må skrive hele kommandoer manuelt.
**Filer:** `userspace/shell/src/main.rs`

#### 3.4 Shmem Safety
**Status:** DELVIS
**Problem:** Ingen bounds-checking for shmem-mapping. En task kan mappe shmem oppå en annen tasks kode. Ingen capability revocation.
**Filer:** `kernel/src/ipc/shared_memory.rs`

---

### TIER 4 — Visjon: Det som gjør det til et NEXT-GEN AI OS

#### 4.1 Larger Model Support
**Utfordring:** SmolLM2-135M er 87MB Q4_0. Større modeller (7B+) krever:
- Tensor offloading til disk
- Paged attention (ikke hele KV-cachen i RAM)
- Muligens GPU-akselerasjon

#### 4.2 Voice I/O
**Utfordring:** Ingen audio-driver. For en AI-OS bør brukeren kunne snakke til maskinen.
- HDA-driver for QEMU (`-device intel-hda`)
- Whisper-lignende ASR-modell (eller ekstern mikrofon-stream)
- TTS via en liten vokal-modell

#### 4.3 GPU Compute (Vulkan/OpenCL)
**Utfordring:** All inferens er CPU-basert. GPU ville gi 10-100x speedup.
- VirtIO-GPU driver for QEMU
- Compute shader pipeline for GEMM
- Denne er STOR — månedsvis med arbeid

#### 4.4 Real Hardware Boot
**Utfordring:** Fungerer kun i QEMU. For å boote på ekte hardware trengs:
- SATA/NVMe driver (lagring)
- USB HID driver (tastatur/mus)
- ACPI full implementasjon
- PCI-e device enumeration

#### 4.5 Multi-agent Inference
**Utfordring:** Kun én modell om gangen. For et ekte AI-OS:
- Flere modeller lastet i ulike arenaer
- Routing mellom modeller (Intent Service)
- Spesialist-modeller (kode, matematikk, kreativ)

#### 4.6 Continuous Learning
**Utfordring:** Modellen er fryst etter trening. For et levende AI-OS:
- LoRA/QLoRA fine-tuning på device
- Personlig kontekst (brukerpreferanser i SQLite)
- Semantic memory via embedding-søk (allerede delvis i Synapse)

---

## Anbefalt Rekkefølge

```
FASE 1: Stabilitet (1-2 uker)
  1.1 kill_task / exit_task
  1.2 IPC timeout
  1.3 Heap memory pressure detection
  1.4 Re-enable buddy allocator

FASE 2: Persistens (1 uke)
  2.1 File write via Synapse
  2.4 Clean shutdown

FASE 3: Åpenhet (1 uke)
  2.3 HTTP server for inference API
  2.2 Multi-modell loading fra kommandolinje

FASE 4: UX Polish (1 uke)
  3.1 Clipboard
  3.2 Scrollback (512 linjer)
  3.3 Kommandohistorikk

FASE 5: Visjon (ongoing)
  4.1-4.6 Avhenger av ambisjonsnivå
```

---

## Perspektiv

Folkering OS har oppnådd noe bemerkelsesverdig: et **fungerende AI-OS bygget fra scratch på 2 måneder**. Inference-motoren, tokenizeren, og telemetri-systemet er av profesjonell kvalitet. Men gapet mellom "demo som fungerer" og "OS man kan stole på" ligger i de grå, kjedelige delene: task lifecycle, filskriving, memory management, og error recovery.

De mest impactfulle neste stegene er **1.1 (kill_task)** og **2.1 (filskriving)** — disse to alene ville gjøre systemet dramatisk mer robust og nyttig.
