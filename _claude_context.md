# Folkering OS — Claude Context

## Project Overview
Rust bare-metal x86-64 hobby OS. Limine bootloader, QEMU for emulation, Docker/WSL build environment.

## Build
- Kernel: `kernel/` → `target/x86_64-folkering/release/kernel` (ELF)
- Userspace: `userspace/` → `target/x86_64-folkering-userspace/release/{compositor,shell,synapse}` (ELF)
- Build runs in WSL (Ubuntu-22.04), outputs to Windows filesystem via `/mnt/c/`

## Debug Session
Start QEMU with full debug flags (QMP + GDB stub):
```bash
wsl -e bash -c "cd /mnt/c/Users/merkn/folkering/folkering-os && ./tools/qemu-debug-live.sh"
```
- QMP socket: `/tmp/folkering-qmp.sock` (WSL-native)
- Serial log:  `/tmp/folkering-serial.log`
- GDB stub:    `localhost:1234` (QEMU starts halted — `continue` in GDB or use GDB bridge)

## MCP Debug Server — `mcp/server.py`
Registered in `~/.claude.json` as `folkering-debug`, invoked with `py -3.12`.

### Tool: `kernel_symbol_lookup`
Resolves hex addresses → function name + source location using the kernel/userspace ELF symbols.

**Parameters:**
- `addresses` (required): `["0x205AD5", "0xffffffff80000004"]`
- `elf_path` (optional): Override ELF path (auto-detected from address range by default)

**Auto-detection logic:**
- `0xffffffff80000000–0xffffffffffffffff` → kernel ELF
- `0x200000–0x4fffff` → compositor/shell/synapse (userspace)
- CS=0x08 = kernel mode, CS=0x23 = user mode

**Notes:**
- Release builds have `.symtab` but NO DWARF — `addr2line` returns `??`, `nm` finds nearest symbol
- ELF is copied to `/tmp/folkering-{name}` in WSL for native filesystem speed (avoids slow 9P `/mnt/c/`)

**Example output:**
```
────────────────────────────────────────────────────────────
Address  : 0x205AD5  [compositor]
Symbol   : compositor::main  +0xc46
Source   : (no DWARF — build with debug_assertions or use debug profile)
```

### Tool: `serial_throttle_analyzer`
Reads a serial/QEMU log and collapses repeated loop patterns. Turns millions of repetitive lines into `[LOOP xN]` summaries so anomalies (faults, panics) are immediately visible.

**Parameters:**
- `log_path` (required): Path to log file (e.g. `/tmp/folkering-serial.log`)
- `window` (default: 5): Lines per pattern unit
- `threshold` (default: 10): Repeats before collapsing
- `max_output_lines` (default: 200): Output line cap

**Example usage:** `serial_throttle_analyzer "/tmp/folkering-serial.log"`

### Tool: `qemu_inspect_registers`
Queries live QEMU CPU state via QMP. Returns GPR (RAX–R15, RIP, RSP, RBP, RFLAGS) and segment registers. Optionally includes XMM0–XMM15.

**Parameters:**
- `include_xmm` (default: false): Include SSE registers
- `qmp_socket` (default: `/tmp/folkering-qmp.sock`): Override QMP socket path

**Requires:** QEMU running with `-qmp unix:/tmp/folkering-qmp.sock,server,nowait`

**Implementation note:** QMP socket is WSL-native, so tool embeds a Python client script and runs it via `wsl -d Ubuntu-22.04 python3 -c "..."` (NOT `wsl -e python3` — that fails with E_UNEXPECTED).

**Example output:**
```
QEMU CPU Register State
════════════════════════════════════════════════════════════
RAX=0000000000000000 RBX=0000000000000000 RCX=0000000000000000
RDX=0000000000000663 RSI=0000000000000000 RDI=0000000000000000
...
```

## Skill: ABI Auditor (`/abi-audit`)
Saved at `~/.claude/skills/abi-audit.md`. Activates on:
- Shared `naked_asm!` / `global_asm!` / `.asm` blocks
- "check my abi" / "audit this asm" / `/abi-audit`

Performs SysV AMD64 ABI compliance check:
1. Stack map diagram (RSP offset at each push/pop)
2. 16-byte RSP alignment check before every `call`
3. Callee-saved register clobber detection (RBX, RBP, R12–R15, XMM8–15)
4. Argument register verification (RDI, RSI, RDX, RCX, R8, R9 order)
5. Red zone warning (no red zone in kernel interrupt handlers)

## WSL Invocation Quirks
- `wsl -e bash -c "..."` — works for bash commands (cp, nm, addr2line)
- `wsl -d Ubuntu-22.04 python3 -c "..."` — required for Python; `-e python3` fails with `E_UNEXPECTED`
- ELF operations use `wsl -e bash -c` (bash + binutils)
- QMP client uses `wsl -d Ubuntu-22.04 python3 -c` (Python socket)

## Memory Layout
| Range | Owner |
|-------|-------|
| `0xffffffff80000000–0xffffffffffffffff` | Kernel (high-half) |
| `0x200000–0x4fffff` | Userspace processes |

## Key Files
| File | Purpose |
|------|---------|
| `mcp/server.py` | MCP debug server (3 tools) |
| `tools/qemu-debug-live.sh` | QEMU launch script with QMP+GDB |
| `~/.claude/skills/abi-audit.md` | ABI Auditor skill |
| `kernel/target/x86_64-folkering/release/kernel` | Kernel ELF |
| `userspace/target/x86_64-folkering-userspace/release/compositor` | Compositor ELF |
