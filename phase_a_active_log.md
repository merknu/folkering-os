# Phase A — Draug Process Isolation Migration

**Goal:** Move Draug from inside `compositor` into its own userspace task (`draug-daemon`), so a Draug panic stops being a desktop crash. This is the foundation for B/C/D from the Dora-rs analysis.

**Started:** 2026-05-01

## Architectural target

```
Before:                          After:
┌──────────────────────┐         ┌──────────────────┐  ┌──────────────────┐
│ compositor (task 4)  │         │ compositor (4)   │  │ draug-daemon (7) │
│                      │         │                  │  │                  │
│  GUI/render          │         │  GUI/render only │  │  draug.rs        │
│  WASM runtime        │   →     │  WASM runtime    │  │  draug_async.rs  │
│  Draug state         │         │                  │◄─┤  knowledge_hunt  │
│  Draug async TCP     │         │  IPC client ──► IPC│  agent_planner   │
│  Friction sensor     │         │                  │◄─┤  refactor_loop   │
└──────────────────────┘         └──────────────────┘  └──────────────────┘
                                       │  ▲
                                       │  └─ status shmem (read-only) ─┐
                                       └────► IPC commands ────────────┘
```

## Current Draug coupling (audit)

LOC budget: ~7000 lines to move.

| File | Lines | Destination |
|---|---:|---|
| `compositor/src/draug.rs` | 1844 | `draug-daemon/src/state.rs` (or split) |
| `compositor/src/mcp_handler/draug_async.rs` | 979 | `draug-daemon/src/async_orchestrator.rs` |
| `compositor/src/mcp_handler/agent_logic.rs` | 427 | `draug-daemon/src/agent_logic.rs` |
| `compositor/src/mcp_handler/agent_planner.rs` | 555 | `draug-daemon/src/agent_planner.rs` |
| `compositor/src/mcp_handler/autodream.rs` | 740 | `draug-daemon/src/autodream.rs` |
| `compositor/src/mcp_handler/knowledge_hunt.rs` | 1072 | `draug-daemon/src/knowledge_hunt.rs` |
| `compositor/src/mcp_handler/refactor_loop.rs` | 432 | `draug-daemon/src/refactor_loop.rs` |
| `compositor/src/mcp_handler/task_store.rs` | 369 | `draug-daemon/src/task_store.rs` |
| `compositor/src/mcp_handler/token_stream.rs` | 411 | `draug-daemon/src/token_stream.rs` |
| `compositor/src/refactor_types.rs` | 63 | likely shared in `libfolk::sys::draug` |

### Compositor → Draug call sites (must replace with IPC)

| Site | Today | Replacement |
|---|---|---|
| `main.rs:630` | `DraugDaemon::new()` | (gone — daemon owns its state) |
| `main.rs:632-650` | `draug.restore_state()`, status reads, `draug_bridge_update()` | Read status from shmem; daemon does its own restore |
| `main.rs:669` | `draug.install_refactor_tasks(merged)` | IPC `DraugCommand::InstallRefactorTasks` |
| `main.rs:909` | `draug.last_input_ms()` | Read from status shmem |
| `input_keyboard.rs:79` | `draug.on_user_input(input_ms)` | IPC `DraugCommand::UserInput { ms }` |
| `input_mouse.rs:115` | same | same |
| `rendering/wasm_layer.rs:116,134` | `ctx.draug.record_crash(k)` | IPC `DraugCommand::WasmCrash { key }` |
| `RenderContext { draug }` | mutable ref | replace with status-shmem reader |
| `DispatchContext { draug }` | mutable ref | IPC for any commands; status reads from shmem |

## Subphases

### A.1 — Skeleton crate ⏳
- `userspace/draug-daemon/Cargo.toml`
- `src/main.rs` with heap + IPC dispatch loop (synapse-service template)
- `src/lib.rs`
- Add to workspace members
- Build clean, no logic yet.

### A.2 — IPC protocol
- `libfolk::sys::draug` module: `DraugCommand`, `DraugEvent`, client wrappers
- `DRAUG_TASK_ID` const

### A.3 — Status shmem
- Atomics-based shared region for live counters, mapped read-only by compositor
- Daemon writes, compositor reads. Replaces direct field access from many sites.

### A.4 — Code move
- Move the 9 source files listed above
- Adjust module structure
- Compositor still has thin shims (e.g. `compositor::draug::stub` for old call sites that haven't been rewritten yet)

### A.5 — Compositor IPC client
- Replace direct calls with `libfolk::sys::draug::*` wrappers
- Drop `&mut draug` from RenderContext/DispatchContext/mouse/keyboard
- Compositor compiles WITHOUT the moved code

### A.6 — Kernel boot
- Special-case spawn for `draug-daemon` after Compositor (Task 7?)
- Grant IPC capability
- Verify deterministic task ID

### A.7 — Proxmox smoke test
- Build, repack initrd, deploy to VM 800
- Verify Phase 17 still produces L1 PASSes
- Verify HUD shows live status
- Verify WASM crash recording still works
- Verify friction sensor receives input

## Risk register

- **TCP slot ownership**: smoltcp slot pool is per-task in kernel. Draug-daemon's TCP calls allocate from a different task's pool than compositor's. Should be fine but needs verification.
- **Status shmem race**: counters are atomic stores so single-counter reads are safe, but if compositor reads `passed` and `failed` non-atomically, totals can briefly be inconsistent. Acceptable for status display.
- **Boot ordering**: if compositor starts before draug-daemon and tries to send IPC immediately, IPC fails. Need either (a) compositor to retry, or (b) draug-daemon spawned before compositor.
- **Friction sensor latency**: compositor → IPC → daemon adds ~µs to the hot input path. Negligible but should measure.
- **Restore state contention**: only daemon now owns the Synapse state file. Verify Synapse handles a different task ID as the writer.

## Decision log

- **2026-05-01:** Phase A scope confirmed. Writing skeleton first; not migrating logic until A.1+A.2+A.3 are stable.
- **2026-05-01:** Task ID 7 chosen for draug-daemon (after 4=compositor, 5=intent, 6=inference). Will special-case spawn to make it deterministic.
- **2026-05-01:** Status shmem chosen over IPC-pull for status reads — compositor reads counters every render frame, an IPC roundtrip per frame is wasteful.
