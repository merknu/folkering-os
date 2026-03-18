# Phase 8 GUI Freeze Debugging Log

## Problem Summary
- GUI fryser med #GP fault etter musebevegelse
- RIP: 0x205AD5, CS: 0x23 (userspace)
- Error code: 0x0

## Session 2026-02-01

### Attempt 1: Forenkle shell main loop
- **Endring**: Fjernet recv_async() fra shell main loop, forenklet til bare read_key() + yield_cpu()
- **Fil**: `userspace/shell/src/main.rs`
- **Resultat**: Kompilert OK, men gammel binary ble lastet av kernel

### Attempt 2: Rebuild og oppdater initrd
- **Handling**:
  - `cargo build --release` i userspace
  - `folk-pack create boot/initrd.fpk --add shell:elf:...`
  - `mcopy -i phase8-working.img@@1M initrd.fpk ::initrd.fpk`
- **Resultat**:
  - Lokal initrd.fpk viser shell = 43264 bytes ✓
  - MD5 hash matcher mellom lokal og disk-image
  - MEN: Kernel leser shell som 43352 bytes (gammel størrelse!)

### Key Finding: Struct Layout Mismatch
Kernel rapporterer:
```
[RAMDISK] Address: 0xffff80000fcda000, size: 270361 bytes  <- KORREKT
[RAMDISK] Entry 1: "shell" (ELF, 43352 bytes)              <- FEIL! Skulle vært 43264
```

Lokal initrd.fpk (verifisert med od):
- Shell size felt @ 0xb0: `00 a9 00 00 00 00 00 00` = 0xa900 = 43264 ✓

**Hypotese**: FpkEntry struct i kernel har annen layout enn i folk-pack tool.

### Attempt 3: Undersøk struct layout
- Verifisert at FpkEntry har size 64 og `offset of size: 48` i begge
- Kernel og verktøy er identiske! Struct-hypotese avkreftet.

### Attempt 4: Oppdater kernel binary
- Sammenlignet kernel hash på disk vs lokal: FORSKJELLIGE!
- Oppdaterte kernel til `::boot/kernel.elf`
- **Resultat**: Fortsatt feil - shell leses som 43352

### Attempt 5: Oppdaget feil initrd-plassering
- Limine config: `module_path: boot():/boot/initrd.fpk`
- Jeg kopierte til: `::initrd.fpk` (root)
- Limine laster fra: `::boot/initrd.fpk` (underkatalog)

**Det fantes TO initrd.fpk filer!**
- `::initrd.fpk` - NY (16:13)
- `::boot/initrd.fpk` - GAMMEL (15:43) ← Limine lastet denne!

### LØSNING
```bash
mcopy -o -i disk.img@@1M initrd.fpk ::boot/initrd.fpk
```

### Resultat
- Shell viser nå 43264 bytes (korrekt!)
- `[SHELL] Running (Task 3)` - ny output ✓
- GUI kjører 30+ sekunder uten #GP fault! ✓

---

## Root Cause
mcopy-kommandoen kopierte initrd til feil sted (`::initrd.fpk` i stedet for `::boot/initrd.fpk`). Limine lastet alltid den gamle initrd fra `/boot/initrd.fpk`.

## Filer endret
- `userspace/shell/src/main.rs` - Fjernet recv_async() som forårsaket #GP
- `boot/initrd.fpk` - Ny shell binary (43264 bytes)
- `boot/boot-test-*.img` - Fungerende boot image med riktig kernel + initrd

---

## Session 2 - 2026-02-01 (fortsettelse)

### Problem
GUI frøs fortsatt under musebevegelse, selv etter at shell recv_async() ble fjernet.

### Attempt 6: Deaktiver SSE i userspace target
- **Endring**: La til `"features": "-sse,-sse2,-sse3,-ssse3,-sse4.1,-sse4.2,-avx,-avx2"` i x86_64-folkering-userspace.json
- **Resultat**: Kompilering feilet med error:
  ```
  error: target feature `sse2` is required by the ABI but gets disabled in target spec
  ```
- **Konklusjon**: Kan ikke deaktivere SSE på x86_64 - det er påkrevd av ABI

### Attempt 6b: Prøv soft-float
- **Endring**: Byttet til `"features": "-mmx,+soft-float"`
- **Resultat**: Kompilering feilet:
  ```
  error: target feature `soft-float` is incompatible with the ABI
  ```
- **Konklusjon**: Fjernet features-linjen helt, bruker standard x86_64 features

### Attempt 7: Undersøk syscall-implementasjonen
- **Fil**: `kernel/src/arch/x86_64/syscall.rs`
- **Fokus**: recv_async syscall (0x20) og relaterte syscalls (0x21, 0x22, 0x23)
- **Funn**: Syscall-implementasjonen ser korrekt ut
- **Konklusjon**: Problemet er ikke i syscall-koden

### Attempt 8: Undersøk framebuffer-operasjoner
- **Fil**: `userspace/compositor/src/framebuffer.rs`
- **Fokus**: `save_rect()`, `restore_rect()`, `draw_cursor()` som brukes i musehåndtering
- **Funn**: Koden ser trygg ut med bounds-checking
- **Konklusjon**: Framebuffer-operasjonene er ikke direkte årsak

### Attempt 9: Detaljert analyse av crash-mønster
- **Crash pattern**: `M!!` (musehåndtering) fulgt av #GP
- **RIP**: 0x205ABA
- **CS**: 0x23 (userspace ring 3)
- **Error code**: 0x0 (ingen segment-relatert feil)
- **Observasjon**: Krasjet skjer ETTER at musehendelser er prosessert, ikke under

### Attempt 10: Analysere compositor stack-bruk
- **Fil**: `userspace/compositor/src/main.rs` linje 360-430
- **Lokale variabler i main()**:
  - `cursor_bg: [u32; 192]` = 768 bytes
  - `text_buffer: [u8; 256]` = 256 bytes
  - `cursor_x, cursor_y: i32` = 8 bytes
  - Diverse andre variabler ~100+ bytes
- **Total estimert**: 1.5KB+ bare i lokale variabler
- **Funksjonskall**: `save_rect()`, `restore_rect()`, `draw_cursor()` legger til mer

### KEY FINDING: User Stack Overflow
**Oppdaget at userspace stack er bare 4KB (1 page)!**
- Compositor bruker over 1KB stack-variabler
- Med funksjonskall og lokale variabler kan stacken lett overflomme
- #GP med error code 0x0 er typisk for stack overflow i x86_64

### Løsning: Øk stack-størrelse til 16KB
**Endringer i `kernel/src/arch/x86_64/usermode.rs`:**
1. `allocate_user_stack_at()` - Allokerer nå 4 pages (16KB) i stedet for 1 page
2. `allocate_user_stack_in_table()` - Samme endring for task-spesifikke page tables

```rust
// Før: Kun 1 page (4KB)
let stack_page_addr = memory::physical::alloc_page()

// Etter: 4 pages (16KB)
const STACK_PAGES: u64 = 4;
for i in 0..STACK_PAGES {
    let page_base = stack_base + i * 4096;
    let stack_page_addr = memory::physical::alloc_page()
    // ... map each page
}
```

### Test Image
- `boot/phase8-16kb-stack.img` - Boot image med 16KB user stack

### Resultat
**SUKSESS!** Systemet kjører stabilt i 15+ sekunder uten #GP fault.
- Main loop pattern: `MKRIYMKRIY...` (Mouse, Keyboard, Redraw, IPC, Yield)
- Ingen krasj under musehåndtering eller tastaturhåndtering
- 16KB stack er tilstrekkelig for compositor

---

## Konklusjon: Root Cause Analysis

Problemet var **stack overflow** i userspace tasks:

1. **Original stack**: 4KB (1 page)
2. **Compositor stack usage**: ~1KB+ (cursor_bg 768B, text_buffer 256B, osv)
3. **Når**: Musehåndtering krever mange funksjonskall som øker stack usage
4. **Symptom**: #GP fault med error code 0x0 under musebevegelse

**Løsning**: Øk userspace stack til 16KB (4 pages) i `kernel/src/arch/x86_64/usermode.rs`

---

## Filer endret (Session 2)

| Fil | Endring |
|-----|---------|
| `kernel/src/arch/x86_64/usermode.rs` | Økt stack fra 4KB til 16KB (4 pages) |
| `userspace/x86_64-folkering-userspace.json` | Testet SSE/soft-float (tilbakestilt) |
| `boot/phase8-16kb-stack.img` | Nytt boot image med fiksen |
| `boot/initrd.fpk` | Oppdatert med nyeste binaries |

## Verifisering

```bash
# Kjør QEMU for å teste
"/c/Program Files/qemu/qemu-system-x86_64.exe" \
  -drive format=raw,file="boot/phase8-16kb-stack.img" \
  -m 256M -serial stdio -no-reboot
```

Forventet resultat:
- Mus kan beveges fritt uten frysing
- Tastatur fungerer i omnibar
- Main loop viser `MKRIYMKRIY...` i serial output

---

## Session 3 - 2026-02-01 (fortsettelse)

### Problem
Etter at stack overflow ble fikset, frøs GUI fortsatt - men serial output viste INGEN #GP fault!
Main loop (`MKRIYMKRIY`) kjørte kontinuerlig, men:
- Ingen mus-events (`!` etter `M`)
- Ingen tastatur-events (`!` etter `K`)
- Omnibar synlig, men input funket ikke

### Attempt 11: Analyse av serial output
- Main loop kjørte: `[LOOP]MKRIYMKRIYMKRIY...`
- Ingen `!` tegn = ingen input-events mottatt
- Ingen #GP fault = systemet krasjet ikke, bare ingen input

### Key Finding: Manglende IRQ12 handler i IDT
**Mus-interrupt handler (IRQ12, vector 44) var IKKE registrert i IDT!**

IDT setup hadde kun:
- Timer: vector 32
- Keyboard: vector 33

Men manglet:
- **Mouse: vector 44** (IRQ12)

### Løsning: Legg til mouse interrupt handler
**Endringer i `kernel/src/arch/x86_64/idt.rs`:**

```rust
// I IDT setup:
idt[44].set_handler_fn(mouse_interrupt_handler);

// Ny handler funksjon:
extern "x86-interrupt" fn mouse_interrupt_handler(_stack_frame: InterruptStackFrame) {
    crate::drivers::mouse::handle_interrupt();
}
```

---

## Session 4 - 2026-02-09

### Problem
Keyboard and mouse interrupts not arriving after userspace starts, even though timer interrupt works.

### Key Finding: PIC doesn't work with APIC enabled
When Local APIC is enabled, the legacy PIC->LINT0 "virtual wire mode" doesn't work reliably for device interrupt routing. Timer interrupt works because it uses Local APIC's LVT timer directly.

### Attempt: Implement IOAPIC

Created `kernel/src/arch/x86_64/ioapic.rs`:
- Maps IOAPIC at physical 0xFEC00000 to virtual 0xFFFFFFFFFEC00000
- Configures redirection table entries for IRQ1 (keyboard) and IRQ12 (mouse)
- Sets IMCR (Interrupt Mode Control Register) to disconnect PIC

**Changes Made:**
1. `kernel/src/arch/x86_64/ioapic.rs` - New IOAPIC driver
2. `kernel/src/arch/x86_64/mod.rs` - Added ioapic module
3. `kernel/src/lib.rs` - Initialize IOAPIC, disable PIC
4. `kernel/src/arch/x86_64/apic.rs` - Mask LINT0 (not using virtual wire mode)
5. `kernel/src/drivers/keyboard.rs` - Added `init_without_pic()`, removed PIC EOI
6. `kernel/src/drivers/mouse.rs` - Added `init_without_pic()`, removed PIC EOI

**IOAPIC Configuration Verified:**
```
[IOAPIC] ID=0x0, Version=0x20, Max entries=24
[IOAPIC] Enabled IRQ1 -> Vector 33 (low=0x21, high=0x0)
[IOAPIC] Enabled IRQ12 -> Vector 44 (low=0x2c, high=0x0)
[APIC] Local APIC ID: 0
```

### Current Status
- IOAPIC is initialized and configured correctly
- PIC is fully masked (0xFF on both PICs)
- IMCR set to APIC mode
- Timer interrupts work (preemption happens)
- But keyboard/mouse interrupts still not arriving

### Hypothesis
QEMU may not generate PS/2 interrupts when running in background without actual user input. Need to test with interactive display.

### Next Steps
1. Run QEMU interactively with display and move mouse
2. Or use QEMU monitor to send synthetic keyboard events
3. Consider alternative: Use USB mouse/keyboard instead of PS/2

---

## Session 5 - 2026-03-16 (Systematic Testing + New Kernel Fix)

### Test Scenario Results

| Scenario | Status | Notes |
|----------|--------|-------|
| Boot (phase8-debug.img) | ✅ | Boots to GUI via QEMU |
| MKRIY event loop | ✅ | Serial output confirms main loop running |
| GUI render | ✅ | FOLKERING OS header, Neural Desktop, omnibar visible |
| Keyboard input via QMP | ✅ | `send-key` → kernel interrupt → `K!` in serial → GUI updates |
| Command execution | ✅ | Enter key submits command |
| `calc` command | ✅ | Returns "math evaluation coming soon" |
| Error handling | ✅ | "Unknown command: X", "Type 'help' for available commands" |
| Mouse events (QMP) | ⚠️ | PS/2 mouse detected, rel events accepted, no M! logging (kernel polls, doesn't log mouse events) |
| New kernel (auto mode) | ✅ FIXED | See below |

### Problem: New Kernel Crashed After STI

**Symptom**: `folkering_run("auto")` injected new kernel → QEMU exited immediately

**Root Cause** (3 nested bugs):
1. `iso_root/boot/kernel.elf` was compiled Feb 1 13:14 — an OLD binary using legacy PIC instead of IOAPIC
2. Source code was updated to IOAPIC (session 4, Feb 9) but never recompiled
3. `folkering-mcp/server.py` used wrong binary name `folkering-kernel` instead of `kernel`

**Crash chain**: When STI enabled interrupts, PIC was still active → spurious PIC interrupt fired
on vector without IDT handler → GP fault → double fault → triple fault → QEMU exit (-no-reboot)

**Evidence**: Boot output showed `[PIC] Enabled IRQ1` instead of `[IOAPIC] Enabled IRQ1`

### Fix Applied
1. `cargo clean` in `kernel/` to force full rebuild
2. `cargo +nightly build --release` in `kernel/` → produces `target/x86_64-folkering/release/kernel`
3. Fixed `folkering-mcp/server.py` line 179: `"folkering-kernel"` → `"kernel"`
4. New kernel (1.19MB, 2026-03-16) copied to `iso_root/boot/kernel.elf`

### Known Remaining Issues
- **PS/2 keyboard scan code bug**: Letters f, h, n, s, v, x consistently dropped by kernel
- **Mouse events**: QEMU sends rel events OK, but no visual cursor feedback (kernel may not have cursor rendering)
- **Space key**: `spc` QMP keycode doesn't register in kernel

---

## Session 6 - 2026-03-16 (Keyboard Mystery Solved)

### Problem
After new IOAPIC kernel deployed, keyboard interrupts appeared to stop working — no `!` in serial output after QMP `send-key`.

### Investigation

#### QEMU-side verification
- `info irq`: IOAPIC IRQ1 count incremented with each key press ✓
- `info lapic`: IRR=(none) even after key press — looked like interrupt not reaching CPU
- `info pic`: IOAPIC pin 1 correctly configured: vec=33, unmasked, edge, physical dest=0 ✓
- PIC0 irr=13 shows IRQ1 pending but masked (expected) ✓

#### Root Cause
**Stale serial logger processes consuming all serial output.**

When `folkering_run()` starts QEMU, it spawns a Python subprocess to read serial TCP port 4444 and write to `serial.log`. When QEMU was stopped (but subprocess not killed), the Python process kept the TCP connection open. On next QEMU start, the new serial logger couldn't connect (connection refused or stale connection consumed data).

Keyboard interrupts WERE firing — `info irq` showed IRQ1 count incrementing. But the `!` characters were going to the stale logger process (or being buffered) instead of to our test connection.

**Evidence**: Direct Python socket connection to port 4444 (bypassing stale loggers) showed:
```
RIYMKRIYMK![IRQ1][SC:0x1e][K:a]RIY[KEY:2:a]aMKRIYMKRIYM
```
Full keyboard pipeline working: IRQ → scancode → ASCII → compositor routing → GUI display.

### Fix
Updated `folkering-mcp/server.py`:
1. `_save_state()` now stores `logger_pid` alongside QEMU PID
2. `folkering_stop()` kills serial logger subprocess before stopping QEMU
3. `folkering_run()` kills previous serial logger before starting new one

### Current System Status (phase8-debug.img + new IOAPIC kernel)

| Scenario | Status | Notes |
|----------|--------|-------|
| Boot | ✅ | Limine → kernel → GUI |
| MKRIY loop | ✅ | Continuous main event loop |
| Timer/preemption | ✅ | APIC LVT timer → preemptive scheduling |
| Keyboard via QMP | ✅ | IOAPIC IRQ1 → IDT[33] → ASCII → compositor |
| Letter 'h' | ✅ | Previously thought dropped — was working all along |
| Command routing | ✅ | `[KEY:2:h]`, `[KEY:3:e]` etc. |
| PS/2 keyboard scan code bug | ✅ RESOLVED | Was false alarm — all letters (f,h,n,s,v,x) work fine |
| Space key (`spc`) | ✅ RESOLVED | `spc` → `[K: ]` works correctly |
| Mouse cursor rendering | ❌ | No visual cursor (architectural gap) |

---

## Session 7 - 2026-03-16 (compositor #PF crash + callee-saved register corruption)

### Problem: Compositor crashes with #PF at CR2: 0x1003E8610

**Symptom:** Page fault (error code 0x6 = write, user-mode, not present) at RIP: 0x206029 during the first omnibar draw. CR2 = 0x1003E8610, which is **past the end** of the mapped framebuffer (mapped at virt 0x100000000, size 1280×800×4 = 0x190000 → end at 0x100190000).

**Disassembly of crash site (compositor binary):**
- 0x206029: `movl $0x333333, (%rsi)` — inner loop pixel write, rsi = row_start_ptr
- 0x20603d: `cmpq %r14, %rcx` — outer loop back-edge, r14 = end_y, rcx = current row
- r14 loaded once at 0x205d3a: `movq 0x210(%rsp), %r14` (from inlined fill_rect code, before loop)
- row_start_ptr loaded at 0x205ff9: `movq 0x1a8(%rsp), %rax`

**Root cause analysis:**
- `fill_rect` was inlined into the compositor's main loop
- Compiler kept `end_y` in r14 across multiple iterations
- When compositor called `write_str` (loops over chars, calls syscall 9 per char), `syscall_entry` in the kernel used r14 as a scratch register (to hold user RSP for stack pivot) and overwrote it
- On return from syscall, r14 no longer contained `end_y` — contained user RSP instead
- Outer loop ran past framebuffer end → #PF

**Secondary evidence (col=260, not 388):**
- Expected start_x=388 (omnibar glyph position)
- Actual crash at col=260 (wrong start_x baked into row_start_ptr)
- Means `row_start_ptr` was also computed from corrupted register data
- Multiple callee-saved registers corrupted, not just r14

### Attempt 1: USER_R14_SAVE static fix

**What:** Added `USER_R14_SAVE: AtomicU64` global in `kernel/src/arch/x86_64/syscall.rs`. At `syscall_entry`, save user r14 to the static before clobbering r14 with user RSP. Restore into Context.r14 when saving the full GPR context.

```asm
// Entry: save user r14 before use
"push rax",
"mov rax, r14",
"mov qword ptr [rip + {user_r14_save}], rax",
"pop rax",
"mov r14, rsp",   // r14 = user RSP (needed for stack pivot)

// Context save: restore real r14
"push rbx",
"mov rbx, qword ptr [rip + {user_r14_save}]",
"mov [r15 + 112], rbx",   // Context.r14 = original user r14
"pop rbx",
```

**Result:** ❌ Crash still occurred. Binary disassembly confirmed fix compiled correctly (store at ffffffff800005a4, restore at ffffffff800006f1). But col=260 vs col=388 proves OTHER callee-saved registers also corrupted (not just r14). The fix was necessary but insufficient.

### Attempt 2: `#[inline(never)]` on fill_rect (WORKAROUND — CURRENTLY ACTIVE)

**What:** Added `#[inline(never)]` to `fill_rect` in `userspace/compositor/src/framebuffer.rs` line 110.

```rust
#[inline(never)]
pub fn fill_rect(&mut self, x: usize, y: usize, w: usize, h: usize, color: u32) {
```

**Why it works:** Forces the compiler to give `fill_rect` its own stack frame. `end_y` and `row_start_ptr` are computed fresh inside fill_rect's frame instead of being stored in callee-saved registers in the outer main loop context. When a syscall corrupts callee-saved registers in fill_rect's frame, fill_rect recomputes them on the next iteration from stack-local variables.

**Result:** ✅ Crash eliminated. Compositor runs stably. Serial shows LA→LB→LC→LD→LE loop repeating. Screenshot renders at ~6KB.

### Real fix required (TODO)

The underlying issue is `syscall_entry` corrupting callee-saved registers (SysV ABI: rbx, rbp, r12, r13, r14, r15) that the compiler assumes are preserved across function calls. The r14 fix only covers r14. A complete audit is needed:

1. List every callee-saved register used in syscall_entry's naked asm **before** the full 16-register GPR push sequence
2. For each such register, add a save slot (global static or per-CPU) or reorganize to only use caller-saved registers (rax, rcx, rdx, rsi, rdi, r8–r11) in that prologue window
3. Re-enable FXSAVE/FXRSTOR for XMM0–XMM15 (currently disabled)
4. After fix: remove `#[inline(never)]` from fill_rect and verify crash is gone

**Key Context struct offsets:**
```
rsp@0, rbp@8, rax@16, rbx@24, rcx@32, rdx@40,
rsi@48, rdi@56, r8@64, r9@72, r10@80, r11@88,
r12@96, r13@104, r14@112(0x70), r15@120,
rip@128, rflags@136, cs@144, ss@152
```

**Acceptance criteria:** Remove `#[inline(never)]` from fill_rect → rebuild → move mouse for 30s without #PF, cursor_y stays in bounds.

---

## Problem: cursor_y Corruption to 0xFFFF_EFC0 (-67136)

**Context:** Same underlying cause as the fill_rect #PF. During preemption while compositor is in the mouse event handler, cursor_y (stored in a callee-saved register) gets corrupted.

**Workaround active:** Bounds-check reset at start of mouse event loop: if `cursor_y > height`, reset to `height/2`.

**Real fix:** Same callee-saved register audit as above + re-enable FXSAVE/FXRSTOR.

---

## Problem: FXSAVE/FXRSTOR Disabled

**Status:** ⚠️ Disabled during debugging. Must re-enable once GPR fix is complete.

If Rust auto-vectorizes any compositor code (memory copies, arithmetic), XMM0–XMM15 will leak between tasks during context switches.

**Files:** `kernel/src/task/preempt.rs`, `kernel/src/main.rs` (irq_timer naked asm)

---

## Current Status (2026-03-16 end of Session 7)

| Problem | Status | Workaround |
|---------|--------|------------|
| #PF in fill_rect | 🟡 Workaround active | `#[inline(never)]` on fill_rect |
| cursor_y corruption | 🟡 Workaround active | Bounds-check reset in mouse loop |
| FXSAVE disabled | ⚠️ Pending | None |
| Debug serial markers (LA–LH) in compositor | ⚠️ Pending removal | Active but noisy |

---

## Session 8 - 2026-03-16 (context.cs = 0x10 at iretq, preemption #GP)

### Problem

Preemptive context switch from Task 2 → Task 3 (count=2) triggers #GP.
`gp_handler` shows the CS field of the iretq frame as 0x10 (kernel data), should be 0x23 (user code).

### Key Observations

- `[PREEMPT] Task 2 -> Task 3 (count=2)` fires — preempt handler IS running.
- `[CTX_PTR]` diagnostic (preempt.rs line 210, after second `next_task.lock()`) **never fires** — not even for count=1 (which succeeds).
- `gp_handler` shows `DEBUG_MARKER = 0xDEAD` (set by `restore_context_only` for initial switch) even though preempt.rs stores 0xAA01–0xAA0B.
- gp_handler shows multiple #GPs with RSP decreasing by 0x100 each (unexplained — `cli;hlt` should stop after first).
- Between `[PREEMPT]` print (line 161) and `[CTX_PTR]` print (line 210) there are **zero serial prints** — we are completely blind.
- `Task::new()` verified to set `context.cs = 0x23`, `context.ss = 0x1B` (checked by `insert_task` serial print).
- `irq_timer` asm reads `[r11+144]` as CS → writes to interrupt frame `[rsp+8]` (correct offset for Context.cs = +144).

### Confirmed Facts

- GDT user code = 0x23 (valid).
- Context struct CS offset = +144 (compile-time asserted).
- `switch_to()` calls `set_current_task()` before `restore_context_only()` → `current_id` never 0 at timer.
- `statistics::record_preemption/context_switch` run before [PREEMPT] print → no fault there.
- Serial I/O (I/O port 0x3F8) bypasses page tables → serial should work even after `switch_page_table`.

### Dead Zone Analysis

Code path between [PREEMPT] and [CTX_PTR] with NO serial prints:

```
line 164: DEBUG_MARKER = 0xAA01         ← silent
line 167: get_task(next_id)             ← silent
line 176: DEBUG_MARKER = 0xAA02         ← silent
line 179: next_task.lock() → page_table_phys, release  ← silent
line 184: DEBUG_MARKER = 0xAA03         ← silent
line 188: switch_page_table()           ← TOP SUSPECT (changes CR3)
line 192: DEBUG_MARKER = 0xAA04         ← silent
line 195: set_current_task(next_id)     ← silent
line 197: DEBUG_MARKER = 0xAA05         ← silent
line 201: DEBUG_MARKER = 0xAA06         ← silent
line 203: next_task.lock() again        ← silent
line 205: DEBUG_MARKER = 0xAA07         ← silent
line 210: [CTX_PTR] print               ← NEVER FIRES
```

### Hypotheses

1. **switch_page_table corrupts kernel static mapping**: After CR3 switch, `DEBUG_MARKER` static in BSS maps to a *different physical page* in new task's PT. gp_handler reads original page = 0xDEAD. Execution continues but serial is lost too.
2. **switch_page_table faults**: New task's PT doesn't map kernel code → #PF after switch → triple fault (no serial because #PF handler not mapped either). But QEMU doesn't restart (suggesting no triple fault).
3. **second next_task.lock() deadlocks**: Spinlock already held (if first lock wasn't released). But [CTX_PTR] doesn't fire even for count=1 which succeeds.
4. **Serial output IS generated but MCP reader loses it**: Less likely since serial is via TCP with OS-level buffering.

### Attempt 1: Add serial prints at every AA0x step (CURRENT)

**Goal:** Determine exactly where execution stops between [PREEMPT] and [CTX_PTR].

**Changes:**
- `kernel/src/task/preempt.rs`: Add serial prints after each AA0x marker and before return.
- `kernel/src/main.rs`: Add `debug_after_preempt_handler(ctx)` function + call from irq_timer asm after `mov r11, rax`.

**Result:** PENDING BUILD
