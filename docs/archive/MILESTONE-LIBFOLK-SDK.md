# Milestone: Userspace SDK (libfolk) & First Rust Shell

**Date**: 27. januar 2026, kl. 08:56
**Commit**: `eff4cfb` on `ai-native-os`
**Status**: COMPLETE

---

## Hva ble oppnådd

For første gang kjører Folkering OS et **ekte Rust-program i userspace**. Ikke håndskrevet assembly, ikke stubber — et fullt Rust-program med `println!`, `format_args!`, og safe syscall-wrappers som kompilerer til en 12 KB ELF-binær og kjører i Ring 3.

Skjermutskrift fra QEMU:

```
Folkering Shell v0.1.0 (PID: 5)
Type 'help' for available commands.

folk>
```

Det er bare tre linjer, men de representerer en fullstendig kjede:

1. Kernel parser ELF-headere
2. PT_LOAD-segmenter mappes til task-ens adresserom
3. Brukerstakk allokeres
4. Task opprettes med riktig entry point
5. Scheduler kjører tasken i Ring 3
6. `println!` bruker `write_char`-syscall per byte
7. `read_key` poller tastatur via syscall
8. `yield_cpu` gir fra seg CPU når ingen tast er trykket

---

## Hva ble bygget

### libfolk — Userspace SDK (`userspace/libfolk/`)

Et `#![no_std]` Rust-bibliotek som gir alt man trenger for å skrive userspace-programmer:

| Modul | Innhold |
|-------|---------|
| `syscall.rs` | Rå `syscall0`–`syscall6` via x86-64 SYSCALL-instruksjonen |
| `sys/task.rs` | `exit`, `yield_cpu`, `get_pid`, `spawn` |
| `sys/io.rs` | `read_key`, `write_char`, `write_str` |
| `sys/ipc.rs` | `send`, `receive`, `reply` med safe error-typer |
| `sys/memory.rs` | `shmem_create`, `shmem_map` |
| `sys/system.rs` | `task_list`, `uptime` |
| `entry.rs` | `entry!`-makro for `_start` + panic handler |
| `fmt.rs` | `print!` / `println!` via `core::fmt::Write` |

### Shell (`userspace/shell/`)

Første applikasjon bygget med libfolk:

- Interaktivt shell med `folk>` prompt
- Kommandoer: `help`, `echo`, `ps`, `uptime`, `pid`, `clear`, `exit`
- Linjebuffer med backspace-støtte
- Kompilerer til 12 KB statisk linket ELF64

### Kernel-endringer

- **ELF-loader** (`spawn.rs`): Full parsing og lasting av PT_LOAD-segmenter — allokerer fysiske sider, mapper til task page table, kopierer data
- **Boot-integrasjon** (`lib.rs`): Shell ELF innebygd via `include_bytes!` og spawnet som Task 5
- **Syscall-fix** (`syscall.rs`): Fikset korrupsjon av R12/R13-registre i syscall entry — bruker-verdiene ble overskrevet av lagret RIP/RFLAGS

---

## Bugfiksen

Den mest kritiske endringen var en **registerkorruppsjons-bug i syscall-entry**:

```asm
// FØR (FEIL):
mov r12, rcx    // R12 = user RIP — overskriver user R12!
mov r13, r11    // R13 = user RFLAGS — overskriver user R13!
// ...
mov [ctx + 96], r12   // Lagrer user RIP som "user R12" — FEIL
mov [ctx + 104], r13  // Lagrer user RFLAGS som "user R13" — FEIL
```

Etter en syscall fikk brukerprogrammet RFLAGS tilbake i R13 i stedet for den faktiske R13-verdien. For assembly-programmene som ikke brukte R12/R13 var dette usynlig. Men Rust-kompileren bruker R13 aktivt (callee-saved register for streng-pekere i løkker), og programmet krasjet umiddelbart.

**Løsning**: Lagre user R12/R13 til statiske variabler *før* de overskrives, og bruke de lagrede verdiene ved Context-save.

---

## Byggesystem

```
userspace/
├── Cargo.toml                          # Workspace
├── .cargo/config.toml                  # build-std = ["core"]
├── x86_64-folkering-userspace.json     # Custom target
├── libfolk/                            # SDK-bibliotek
└── shell/                              # Første app
```

Bygg: `cd userspace && cargo build --release`

Target-spec matcher kernel-ens (`disable-redzone`, `panic=abort`, `static` relocation), men bruker `code-model: small` (vs `large` for kernel).

---

## Hva dette muliggjør

Libfolk er fundamentet for hele userspace-økosystemet. Med dette på plass kan vi nå bygge:

- **Tjenester** (device drivers, filesystem, network) som separate Rust-programmer
- **Init-system** som spawner tjenester via `spawn()`
- **IPC-baserte protokoller** mellom tjenester
- **Brukerverktøy** (ls, cat, osv.) med full `println!`-støtte

Fra håndskrevet assembly til `println!("Hello from Folkering OS!")` — det er et paradigmeskifte.

---

## Filer endret

| Fil | Handling |
|-----|----------|
| `userspace/x86_64-folkering-userspace.json` | Ny |
| `userspace/Cargo.toml` | Ny |
| `userspace/.cargo/config.toml` | Ny |
| `userspace/libfolk/**` (12 filer) | Ny |
| `userspace/shell/**` (2 filer) | Ny |
| `kernel/src/task/spawn.rs` | Endret — ELF segment loading |
| `kernel/src/lib.rs` | Endret — embed & spawn shell |
| `kernel/src/arch/x86_64/syscall.rs` | Endret — R12/R13-fix |

**Totalt**: 19 filer, +964 / -73 linjer
