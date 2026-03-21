# Folkering OS — Claude Context

## Project Overview
Rust bare-metal x86-64 AI-native microkernel OS. Limine bootloader, QEMU for emulation, WSL build.

**Current milestone**: 4.5 (App Weaver + interactive buttons)

## What Works (as of 2026-03-18)
- Graphical desktop (Neural Desktop) with draggable windows
- Interactive terminal: `ls`, `ps`, `cat`, `find`, `uptime`, `app`, `help`
- SQLite VFS via Synapse (custom no_std B-tree reader)
- App Weaver: Shell builds declarative UI → shmem → Compositor renders
- Clickable buttons with action_id IPC events back to owner task
- SYS_MMAP/MUNMAP for dynamic anonymous memory
- Full microkernel IPC: Compositor → Intent Service → Shell → Synapse

## 5 Userspace Tasks
| Task | Name | Purpose |
|------|------|---------|
| 1 | idle | Idle loop |
| 2 | synapse | SQLite, file cache, search |
| 3 | shell | Commands, app builder |
| 4 | compositor | GUI, windows, widgets |
| 5 | intent-service | Capability-based IPC routing |

## Critical Lessons (from this session)
1. **shmem_map MUST use task PML4** — `map_page_in_table(task.page_table_phys, ...)` NOT global MAPPER
2. **receive() truncates to 32 bits** — use recv_async() for full 64-bit payloads
3. **B-tree right_pointer: follow exactly once** — check `next_cell == cell_count`
4. **Avoid IPC deadlocks** — Shell can't send IPC to Compositor while Compositor waits for Shell

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
