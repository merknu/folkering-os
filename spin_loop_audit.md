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

## Pass 2 — deeper sweep (network, TLS, GPU, ACPI)

Re-ran with broader patterns: `while !.*\.load`, `while \w+ [!=<>]+`, `read_volatile`, `mmio_read`, every `loop {}` in kernel/src/net/. Verified each match against actual code rather than sampling. Three additional findings, all in network code:

### 9. `kernel/src/drivers/virtio_net/mod.rs:64` — PCI cap chain walk — **NEW**
- **Severity:** LOW (real PCI hardware doesn't loop) but trivial fix.
- **Pattern:** `while ptr != 0 { ... ptr = next; }` with no cycle detection.
- **Reference:** `virtio_gpu/pci_setup.rs:29` already caps at `iterations < 32`.
- **Fix:** mirror that — `while ptr != 0 && iterations < 32`.

### 10. `kernel/src/drivers/virtio_net/mod.rs:239` — `poll_rx` packet drain — **NEW**
- **Severity:** MEDIUM (broadcast storm / flood DoS).
- **Pattern:** `while let Some((frame, len)) = rx::receive_packet_inner(dev) { ... }` — no cap. RX queue is 1024 deep but a continuous arrival rate exceeding our drain rate keeps the loop going.
- **Fix:** 256 packets per poll; yield back so other ISR-driven work makes progress.

### 11. `kernel/src/net/device.rs:82` — firewall-drop drain — **NEW**
- **Severity:** MEDIUM (deliberate flood with denied packets pins smoltcp).
- **Pattern:** `loop { virtio_net::receive_raw() ... if firewall.allow { return } }` with no cap. If every packet is dropped by firewall, the loop eats whatever the queue serves until it empties — but a faster flood prevents that.
- **Fix:** cap at 256 skipped frames per `receive()` call.

## Verified safe in Pass 2

- All `kernel/src/net/tcp_plain.rs` loops (10s/15s/per-call timeouts)
- All `kernel/src/net/tls/{mod,io}.rs` loops (10s/30s/60s timeouts)
- `kernel/src/net/gemini.rs` loops (60s overall)
- `kernel/src/net/websocket.rs` loops (10s)
- `kernel/src/net/dns.rs:49` (10s timeout)
- `kernel/src/net/udp.rs:95` (per-call timeout)
- `kernel/src/net/a64_stream.rs` all loops (10s timeouts + try_lock cap)
- `kernel/src/net/mod.rs:130` DHCP wait (10s)
- `kernel/src/drivers/nvme.rs:1260` (`for _ in 0..2_000_000`)
- `kernel/src/drivers/virtio_blk.rs:845` (100_000 iter + HLT)
- `kernel/src/drivers/virtio_gpu/flush.rs:253` (timeout + HLT)
- `userspace/compositor/src/main.rs:86` (IPC event loop, yields on WouldBlock)

## Userspace draug-streamer — confirmed earlier observation

`userspace/draug-streamer/src/tcp.rs:69` (and the parallel send_all/recv_exact) — `loop { match tcp_connect_async { TCP_EAGAIN => yield_cpu(), ... } }`. **No timeout.** This is the source of the "ARPs forever" behavior we observed in PR #51. The cleanup-agent (\`trig_01JRiN4Zhpby7Wr8DUHfnn7G\`) is queued to fix it with retry-with-backoff in 14 days; not duplicating the work here.

## Action plan (updated)

- **PR #52 (already filed):** com3_write_byte cap.
- **This PR (extending #52):** + virtio-net PCI cap walk + virtio-net poll_rx + firewall drop drain. Three more network defense-in-depth fixes, all surgical, all building on the same audit lineage.
- **Cleanup-agent territory (2026-05-13):** draug-streamer retry-with-backoff (already in its prompt).
- **Lower priority (someone files an issue first):** cmos.rs RTC waits, input_keyboard/mouse drains, iqe.rs PIT calibration. Each is real but requires more thought (e.g., what's "too long" for an RTC update? does input_keyboard need to sleep 16ms between drains for typematic pacing?).

---

## Key conclusion

The Cloud Hypervisor analysis was correct: this audit found **one new high-severity bug** (com3_write_byte) and **six defense-in-depth opportunities** that could each become a future Issue #49 under a specific environment (network flood, broken RTC virt, COM1 stream). PR #50 was not an isolated incident — it was the most-load-bearing instance of a class of bugs.
