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

### A.1 — Skeleton crate ✅ (commit `87…` on `refactor/phase-a-draug-isolation`)
- `userspace/draug-daemon/Cargo.toml`
- `src/main.rs` with heap + IPC dispatch loop (synapse-service template)
- `src/lib.rs`
- Added to workspace members
- Builds clean to a no_std binary.

### A.2 — IPC protocol ✅
- `libfolk::sys::draug` module with `DRAUG_TASK_ID = 7`, op codes, client wrappers
- Wire format: payload0-only (recv_async drops payload1) → 16-bit op + 48-bit data
- Ops: PING, USER_INPUT, WASM_CRASH, INSTALL_REFACTOR_TASKS, GET_STATUS_HANDLE
- Server-side decoders: `unpack_op`, `unpack_data48`, `unpack_shmem_size`
- Daemon dispatches all five ops, replies with version/handle/OK/ERR

### A.3 — Status shmem ✅
- `DraugStatus` struct (128 bytes, repr C, atomic fields) in libfolk::sys::draug
- Layout version guard so old readers fail loudly on protocol changes
- Daemon allocates 256-byte shmem region at boot, initialises layout_version + INITIALISED flag, grants compositor read access
- `DRAUG_OP_GET_STATUS_HANDLE` for compositor to bootstrap the mapping
- `attach_status() -> Result<&'static DraugStatus, AttachError>` client helper that pings, fetches handle, maps at `0x33000000`, validates layout, returns a static ref
- Daemon vaddr `0x40000000` (well above heap, no collisions)
- Cross-field reads can disagree by ~1 (HUD-acceptable); per-field reads consistent

### A.4 — Code move
- Move the 9 source files listed above
- Adjust module structure
- Compositor still has thin shims (e.g. `compositor::draug::stub` for old call sites that haven't been rewritten yet)

#### A.4 dependency audit (2026-05-01)

Started with `refactor_types.rs` (63 LOC, no deps) as a proof-of-concept for the move pattern. **Pattern works:** add `draug-daemon` as path dep on compositor, move file content to daemon, leave compositor with a `pub use draug_daemon::X::*;` shim. Both crates build clean.

**Discovered during planning:** `mcp_handler/*.rs` files are NOT cleanly movable. They live in compositor's *binary* and import compositor *lib* types deeply:

| File | Imports compositor lib types? | Move complexity |
|---|---|---|
| `task_store.rs` | minimal — only refactor_types | LOW (pure data) |
| `token_stream.rs` | TokenRing + WindowManager + FramebufferView | STAYS in compositor (UI work) |
| `agent_logic.rs` | DraugDaemon + WasmState + WindowManager | MEDIUM — orchestration wrapper |
| `agent_planner.rs` | DraugDaemon, no UI types | LOW |
| `autodream.rs` | WasmState (cache eviction) | MEDIUM |
| `knowledge_hunt.rs` | DraugDaemon | LOW |
| `refactor_loop.rs` | DraugDaemon | LOW |
| `draug_async.rs` | DraugDaemon, AsyncOp, AsyncPhase | LOW (already self-contained) |
| `draug.rs` (lib) | none non-libfolk | LOW |

**Net:** ~5500 of the 7000 LOC are cleanly movable. ~1500 LOC (token_stream + parts of agent_logic, autodream) need to stay in compositor or be split.

**Strategy revision:** A.4 becomes "move the cleanly-movable files first; defer the orchestration files until A.5 has rewired the call sites." This means A.4 lands in two parts:
- **A.4a (this session):** `refactor_types.rs` (DONE), `task_store.rs`, `agent_planner.rs`, `refactor_loop.rs`, `knowledge_hunt.rs`, `draug.rs`, `draug_async.rs` — all the pure-agent + data files. ~5500 LOC.
- **A.4b (after A.5):** the orchestration glue (`agent_logic.rs`, `autodream.rs`) once compositor's tick loop no longer drives them directly.

#### A.4a — completed 2026-05-01

Final code distribution after A.4a:

| Crate | File | LOC |
|---|---|---:|
| draug-daemon | draug.rs | 1845 |
| draug-daemon | draug_async.rs | 977 |
| draug-daemon | knowledge_hunt.rs | 1070 |
| draug-daemon | agent_planner.rs | 553 |
| draug-daemon | refactor_loop.rs | 430 |
| draug-daemon | task_store.rs | 366 |
| draug-daemon | refactor_types.rs | 64 |
| draug-daemon | main.rs | 251 |
| draug-daemon | lib.rs | 28 |
| **draug-daemon TOTAL** |  | **5584** |
| compositor | draug.rs (shim) | 7 |
| compositor | refactor_types.rs (shim) | 9 |
| compositor | mcp_handler/{4 shims} | 23 |
| compositor | mcp_handler/agent_logic.rs (stays) | 427 |
| compositor | mcp_handler/autodream.rs (stays) | 740 |
| compositor | mcp_handler/token_stream.rs (stays) | 411 |
| compositor | mcp_handler/mod.rs (stays) | 116 |

Each commit on `refactor/phase-a-draug-isolation` builds clean. PR #70 (draft) tracks the foundation; A.4a commits are pushed to the same branch so the PR diff grows incrementally.

Net: ~5500 LOC of agent code now lives in its own crate. Compositor is 7 thin shims away from being able to drop the dep entirely (in A.5).

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
