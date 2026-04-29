# Spin-Loop Audit — Folkering OS

**Trigger:** Issue #49 root-cause analysis (PR #50). The `god_mode::poll_com3` unbounded `while let Some(_) = com3_read()` deadlocked the compositor's main loop on emulated 16550 UARTs that report LSR DR=1 indefinitely (default state on QEMU+WHPX/KVM with no COM3 backend).

**Pattern recognition (per Cloud Hypervisor architectural analysis, 2026-04-30):**
> Every place in Folkering that spins on a hardware register or polls without a budget is a latent Issue #49.

**Audit scope:** kernel/src/, userspace/{compositor,libfolk,shell,...}/, drivers, syscall handlers. Skipped apps/ (sandboxed WASM).

**Verdict scale:**
- ✅ **SAFE** — explicit cap, finite-bounded, or HLT-yielding by design.
- 🚩 **UNSAFE** — unbounded poll over a hardware register or syscall that can produce data forever.
- ⚠️ **REVIEW** — has a timeout but it's huge or the wakeup path is unclear.
- 📘 **REFERENCE** — existing-good pattern.

---

## 📘 Reference patterns (calibration)

| Site | Pattern | Cap |
|---|---|---|
| `kernel/src/drivers/serial.rs:170` `com2_async_poll` | RX drain | 4096 iters |
| `kernel/src/drivers/serial.rs:67-78` `com2_write` | TX-empty wait | 1_000_000 wait counter |
| `kernel/src/drivers/nvme.rs:781` NVMe completion | phase-bit poll | `MAX_POLL_ITERS`, fast→HLT, controller-fatal check |
| `kernel/src/drivers/virtio_blk.rs:680` block I/O | ISR poll | 500_000 (~500ms) |
| `kernel/src/drivers/keyboard.rs:377` `read_key_blocking` | block on key | HLT yield, not busy-spin |

Established pattern: every hardware poll either (a) caps iterations, OR (b) yields via HLT. Unbounded busy-spin is the bug.

---

## 🚩 Unsafe findings

### 1. `userspace/compositor/src/god_mode.rs:15` — `poll_com3` RX drain
- **Status:** Fixed in PR #50 (4096-iter cap). Issue #49 root cause.
- **Reference fix:** mirrors `com2_async_poll`.

### 2. `kernel/src/drivers/serial.rs:42` — `com3_write_byte` TX-empty wait — **NEW**
- **Severity:** HIGH
- **Pattern:** unbounded `loop { let lsr = port.read(); if lsr & 0x20 != 0 { break; } }` waiting for the COM3 TX-empty bit.
- **Why:** Identical bug class to PR #50 but on the WRITE side. **Same author capped COM2 write at line 67-78** with the comment *"Safety timeout — don't hang forever"* and the same omission on COM3.
- **Blast radius:** Anything that calls `com3_write` (telemetry, IQE event recording, TIMING reports). A single TX-empty hang freezes the kernel.
- **Fix:** mirror the COM2 pattern — `wait > 1_000_000 { break }` safety timeout.

### 3. `kernel/src/drivers/cmos.rs:50` — `read_rtc` update-in-progress wait
- **Severity:** LOW (RTC virtualization is well-tested everywhere) but kernel-wide blast radius.
- **Pattern:** `loop { if (cmos_read(0x0A) & 0x80) == 0 { break; } }` — no cap.
- **Called from:** `get_rtc()` invoked by compositor's clock-tick path every loop iteration. A stuck RTC freezes everything.
- **Defense-in-depth fix:** add ~10_000-iter cap; if exceeded, return last known time + warn once.

### 4. `kernel/src/drivers/cmos.rs:162` — `write_rtc` same pattern
- Same as #3, less commonly invoked. Same fix shape.

### 5. `userspace/compositor/src/input_keyboard.rs:64` — `while let Some(key) = read_key()`
- **Severity:** MEDIUM
- **Why:** `read_key` syscall (`kernel/src/arch/x86_64/syscall/handlers/io.rs:6`) checks the kernel keyboard buffer AND falls through to `serial::read_byte()` (COM1). On Proxmox/KVM where COM1 is connected to a socat session, a continuous stream of bytes into COM1 (e.g., a misbehaving log forwarder) makes this loop drain indefinitely while blocking the compositor main loop.
- **Defense-in-depth fix:** cap at e.g. 256 keys/iter. Real input bursts are < 50 keys.

### 6. `userspace/compositor/src/input_mouse.rs:90` — `while let Some(event) = read_mouse()`
- **Severity:** MEDIUM
- Same pattern as #5, mouse-side. Kernel maintains a finite ring buffer, but a flooded buffer would still pin the compositor.
- **Fix:** cap at e.g. 256 events/iter.

### 7. `kernel/src/drivers/virtio_net/mod.rs:239` — `poll_rx` packet drain
- **Severity:** MEDIUM
- **Pattern:** `while let Some((frame, len)) = rx::receive_packet_inner(dev) { ... }` with no cap.
- **Why:** under broadcast/multicast storm or a deliberate flood, this drains as long as the RX queue refills. Naturally bounded by RX queue depth (1024) PLUS however many packets arrive during processing — so a fast-enough flood can extend the loop indefinitely.
- **Defense-in-depth fix:** cap at 256 packets per `poll_rx` call (mirror the `com2_async_poll` 4096-but-finite philosophy, scaled down for packet sizes).

### 8. `kernel/src/drivers/iqe.rs:58` — TSC calibration PIT poll
- **Severity:** LOW (boot-only, single call)
- **Pattern:** `loop { let status = port_61.read(); if status & 0x20 != 0 { break; } }` waiting for PIT Channel 2 done bit.
- **Defense-in-depth:** add ~10M-iter cap with fallback to a default `tsc_per_us` if exceeded.

---

## ✅ Verified safe (sampled)

- `kernel/src/drivers/keyboard.rs:377` — `read_key_blocking` uses `hlt()` to yield. Wakes on IRQ. Correct pattern for blocking I/O.
- `kernel/src/drivers/nvme.rs:185` — CAS retry loop on atomic free-mask. Bounded by atomic contention, returns None on exhaustion.
- `kernel/src/drivers/virtio.rs:277` — descriptor chain follow. Bounded by virtqueue size.
- `kernel/src/drivers/cmos.rs:224` — year-from-epoch counter. ~150 iters max.
- `kernel/src/drivers/telemetry.rs:224` — bounded by `max_count`.

---

## Action plan

- **This PR:** apply fix #2 (`com3_write_byte`) — minimal, surgical, mirrors existing COM2 pattern, eliminates kernel-wide hang on broken-COM3-emulator configs. Same risk profile as PR #50.
- **Follow-up issue / PR:** apply caps to #5–#8 as defense-in-depth. #3 and #4 can wait until someone reports an RTC issue or until the cleanup agent's work in 14 days picks them up incidentally.
- **No action needed:** the reference patterns and verified-safe samples — they're calibrated correctly.

---

## Key conclusion

The Cloud Hypervisor analysis was correct: this audit found **one new high-severity bug** (com3_write_byte) and **six defense-in-depth opportunities** that could each become a future Issue #49 under a specific environment (network flood, broken RTC virt, COM1 stream). PR #50 was not an isolated incident — it was the most-load-bearing instance of a class of bugs.
