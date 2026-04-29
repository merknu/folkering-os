# Issue #49 — Draug Tick #1 Freeze Debug Log

## Problem Summary

After `[Draug] Tick #1 | idle: 12s | dreams: 0/10` fires, the kernel goes silent for 5+ minutes. VM stays alive at **CPU = 0% / HLT-idle** (uptime climbs normally) — so it's not a triple-fault or panic. Reproduces on **both WHPX (Windows)** AND **KVM (Proxmox VE)**, so it's a real Folkering scheduling bug, not a hypervisor quirk.

Last serial bytes before silence:
```
[SYNAPSE] write_file: disk flush failed (data in memory only)
[SYNAPSE] Wrote 'draug/refactor_tasks.txt' (1730 bytes, rowid=32, mime=text/plain)
[Draug] Refactor queue: 5 tasks loaded
[LOOP ALIVE]
[IQE-POLL] n=2
[MCP] TimeSyncRequest sent (NTP failed, fallback)
[Draug] Tick #1 | idle: 12s | dreams: 0/10
```

GitHub Issue: https://github.com/merknu/folkering-os/issues/49
Branch: `fix/issue-49-tick-freeze`
Test environment: Proxmox VE 8.4.1 @ 192.168.68.150, VM 800 (`folkering-os`)

## Hypotheses (from issue #49)

1. **Timer IRQ wiring after Tick #1.** Tick #1 fires at uptime 12s, then nothing. Either timer source (LAPIC/PIT/HPET) gets masked, or its handler unmasks itself only on a path the first tick doesn't take.
2. **`should_run_refactor_step` cadence.** Compositor's `tick_idle` only invokes the body when this gates true.
3. **Heartbeat loop conditioned out.** `[LOOP ALIVE]` only printed once.
4. **HLT path not waking on serial RX/timer.** With CPU=0% and serial silent, CPU is parked in HLT.

## Session 2026-04-29

### Attempt 0: Map the code (read-only)
- **Hypothesis/Goal:** Find where `[Draug] Tick #1` prints, what gates Tick #2, and what blocking syscalls are between them.
- **Changes:** None — read-only investigation.
- **Result:**
  - Tick prints at `userspace/compositor/src/mcp_handler/agent_logic.rs:71`. Print condition is `count % 6 == 1 || count <= 3` → Ticks #1, #2, #3 SHOULD all print on cold boot.
  - `Draug::should_tick` (`draug.rs:1001`) requires `self.active && now_ms - last_tick_ms >= 10_000`. Tick interval = 10s. After Tick #1 at uptime ~12s, Tick #2 should fire at uptime ~22s.
  - `now_ms` source: `if tsc_per_us > 0 { rdtsc() / tsc_per_us / 1000 } else { uptime() }`.
  - Main loop (`main.rs:696`) has NO HLT — only `for _ in 0..5_000 { spin_loop() }` when `!did_work`. Spinning means CPU should be 100%, but Proxmox shows CPU=0%. Either compositor is preempted to HLT by kernel, or a syscall is blocking in the kernel.
  - Between Tick #1 and Tick #2 candidates for blocking call:
    - `tick_async` (Phase 13/17 refactor) — calls `fetch_source` SYNCHRONOUSLY at `draug_async.rs:258`. But gated on `should_run_refactor_step` which requires `idle >= 30s`. At uptime 12-15s, idle=12-15s → NOT eligible. Eliminated for the period 12-22s.
    - `knowledge_hunt::run` (`knowledge_hunt.rs:72`) — gated on `idle >= 15s` (KNOWLEDGE_HUNT_IDLE_MS). Eligible from uptime 15s onward. Calls `fbp_request` (synchronous syscall). If KHunt fires and freezes, we'd see `[KGraph] test start` (line 888 of knowledge_hunt.rs) FIRST. We do NOT see it → KHunt is never reached, OR run_graph_supersession_test prints fail somehow. Most likely KHunt iter never executes because freeze happens before idle=15s.
    - `draug_bridge_update` (syscall 0xD0) — atomic stores only, kernel `dispatch.rs:137-153`. Non-blocking. Eliminated.
    - `gpu_flush` (syscall 0x80) — called by `present_and_flush` once per clock tick (every 1s). Has been called ~12 times BEFORE Tick #1 successfully. Not a "first-time" failure, but could degrade.
    - Various input syscalls (mouse/keyboard polls). Typically non-blocking.
  - Synapse persistence is broken on the Proxmox raw 64MB scsi0 image: `[SYNAPSE] VirtIO header read failed: IoError`, `disk flush failed (data in memory only)`. Does NOT block the boot — Synapse falls back to in-memory.
- **Conclusion:**
  - The freeze is most likely between uptime 12s (Tick #1) and 15s (KHunt eligibility) — short 3-second window, no obvious blocking call in the path.
  - OR the freeze is in something running EVERY loop iteration that suddenly fails after Tick #1 has fired.
  - Need actual evidence. Next step: instrument the loop with a periodic heartbeat to prove whether the loop is alive at all after Tick #1.

### Attempt 1: Heartbeat instrumentation
- **Hypothesis/Goal:** Add `[HB iter=N now_ms=M]` print every 5 seconds at the top of the main loop. Two outcomes:
  - **Heartbeats continue but no Tick #2** → loop is alive; freeze is in Draug-tick logic.
  - **Heartbeats stop after some N** → loop is dead; the iteration that printed last heartbeat blocked downstream.
- **Changes:** Added `hb_iter` counter and 5s-throttled `[HB iter=N now=Mms]` print at top of main loop in `userspace/compositor/src/main.rs` right after the existing `[LOOP ALIVE]` one-shot.
- **Side issue found:** `deploy.py` had a bug picking the **oldest** unused disk slot from `qm config` instead of the slot the most recent `qm importdisk` landed in. Caused redeploys to boot the original 8GB blank disk after the first deploy. Fixed: parse the importdisk output for `unusedN:storage:vm-VMID-disk-X`, fall back to highest-numbered unused slot. **Manually re-attached `vm-800-disk-2` to scsi0 to get this experiment running.**
- **Result:** Serial log shows ONE heartbeat at the very moment Tick #1 prints — no second iteration ever runs:
  ```
  582:[BOOT] All tasks spawned, starting scheduler...
  587:[HB] kernel_ticks=0 uptime_ms=10 debug_marker=0xbeef        ← kernel-side, not mine
  768:[HB iter=1 now=12375ms]                                      ← my instrumentation, iter 1
  771:[Draug] Tick #1 | idle: 12s | dreams: 0/10
  ```
  Drained for ~6 minutes — last serial line is `[Draug] Tick #1`. No `[HB iter=2`.
- **Conclusion:**
  - The compositor main loop runs **exactly one iteration** before the freeze. The body of iteration #1 never returns to the top of the loop.
  - The freeze happens AFTER `[HB iter=1]` (top of loop) AND AFTER `[Draug] Tick #1` (printed inside `mcp_handler::tick_ai_systems`).
  - The freeze must be somewhere between Tick #1 print (in `agent_logic::tick`, line 71) and the bottom of the loop body. Candidates remaining inside `tick_ai_systems`:
    - rest of agent_logic::tick (Pattern Mining, Phase 13 refactor, Bridge update — but bridge is called every iter, so first call would have run earlier in the same iteration before the print)
    - Knowledge hunt and AutoDream gates closed at idle=12s
    - MCP `poll()` — `tz_sync_pending` is true at this point because `[MCP] TimeSyncRequest sent` already printed; `poll()` is non-blocking by inspection (`com2_async_poll` is async)
  - Outside `tick_ai_systems`: god_mode::poll_com3, process_mouse, caret blink, process_keyboard, command_dispatch, render_frame, present_and_flush.
  - Need finer markers to localize.

### Attempt 2: Finer-grained markers in main loop body
- **Hypothesis/Goal:** Print short `[L.X]` markers (A through I) at section boundaries inside the main loop, gated on `hb_iter <= 3` so we see which section iteration #1 reaches before freezing.
- **Changes:** Added 9 markers in `main.rs`: pre-aitick, post-aitick, post-com3, pre-mouse, post-mouse, pre-kbd, post-kbd, post-render, post-present.
- **Result:** Serial output before freeze (after first deploy.py bug-fix run):
  ```
  768:[HB iter=1 now=12735ms]
  771:[L.A pre-aitick]
  772:[Draug] Tick #1 | idle: 12s | dreams: 0/10
  773:[L.B post-aitick]
  ```
  After `[L.B post-aitick]` — nothing. NO `[L.C post-com3]`. Drained 5+ minutes.
- **Conclusion:**
  - Freeze is in `god_mode::poll_com3()` at `main.rs` line 871 (call site) → `userspace/compositor/src/god_mode.rs`.
  - Looking at the function: a `while let Some(byte) = libfolk::sys::com3_read() { ... }` with NO iteration cap.
  - On QEMU with no COM3 backend (true on both Proxmox/KVM and the Windows host with WHPX), reading port 0x3E8 with LSR DR=1 indefinitely is a known emulator behavior — kernel `com3_read_byte` faithfully returns `Some(_)` every time, looping forever.
  - The compositor co-author already knew this pattern bites, see `kernel/src/drivers/serial.rs:170` which caps `com2_async_poll` at 4096 reads with the comment "to avoid starving the main loop". COM3 was missed.
  - Fix: cap `poll_com3` at 4096 iterations per call, mirror the COM2 defense.

### Attempt 3: Fix poll_com3 with 4096-iteration cap
- **Hypothesis/Goal:** Replace unbounded `while let Some(byte) = com3_read()` with `for _ in 0..4096 { let Some(byte) = com3_read() else { break }; ... }`. This bounds the worst-case work per frame regardless of what the emulated UART reports, while still draining real injected commands (which are limited to a few hundred bytes anyway).
- **Changes:** Edited `userspace/compositor/src/god_mode.rs:13-31`. Documented why the cap exists (Issue #49 root cause + parallel to com2_async_poll).
- **Result:** Loop runs to completion every iteration:
  ```
  HB iter=1   now=12496ms   (Tick #1 fires here, idle 12s)
  HB iter=69  now=17551ms   (KGraph self-test passes between)
  HB iter=137 now=22557ms   Tick #2 fires (idle 22s)
  HB iter=205 now=27577ms
  ...
  Tick #3 fires at idle 32s → triggers Phase 17 path:
    [Draug-async] fib_iter L1 → LLM
    [Draug-async] connect failed (no slots)   ← expected: no proxy in this VM
    [Draug] Tick #3 | idle: 32s | dreams: 0/10
  ```
  Loop runs at ~70 iter/5s, Phase 17 wiring activates correctly. `connect failed` is environment (no LLM proxy on Proxmox VM 800), not a bug — the code path handles it gracefully via `record_skip`.
- **Conclusion:**
  - Issue #49 ROOT CAUSE FIXED.
  - The single-line semantic change (unbounded `while` → bounded `for`) eliminates the freeze on every QEMU configuration where COM3 has no backend (which is the default for most users).
  - Removed all instrumentation (HB heartbeat + 9 L.X markers); only the actual fix remains.
  - Archived first-boot serial log (with freeze) and fix-verified serial log (with progression) at `proxmox-mcp/serial-logs/`.
  - Side-fix: corrected `proxmox-mcp/deploy.py` to parse `qm importdisk` output for the actual landing slot instead of picking the oldest unused slot.

## Status: RESOLVED (pending PR + commit)
