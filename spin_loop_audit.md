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

## Pass 3 — exhaustive sweep (kernel arch, scheduler, IPC, allocator, drivers, ALL userspace)

Walked every directory in `kernel/src/` (arch/, task/, ipc/, memory/, fs/, jit/, bridge/, capability/, timer/, drivers/{ac97,iommu,iqe,keyboard,mod,mouse,msix,nvme,pci,rng,serial,storage_bench,telemetry,virtio,virtio_blk,virtio_gpu,virtio_net}) and `userspace/{compositor,libfolk,libsqlite,libtensor,inference-server,intent-bus,intent-service,neural-scheduler,shell,synapse,synapse-service,wasm-runtime}`. Searched for `loop {}`, `while let Some`, `while !.*\.load`, `while \w+ [!=<>]+`, `for _ in 0..[huge]`, `try_lock`, `compare_exchange`, `read_volatile`, `mmio_read`. Read every match.

### 12. `kernel/src/drivers/keyboard.rs:170, 194` — boot-time PS/2 buffer drain — **NEW**
- **Severity:** LOW (real PS/2 controllers buffer ≤16 bytes)
- **Pattern:** `while status.read() & 1 != 0 { let _ = data.read(); }` — no cap. Boot freezes if a broken emulator keeps DR=1 high.
- **Sites:** Both `init()` and `init_without_pic()` — duplicated bug.
- **Fix:** 256-read cap on each.

### 13. `kernel/src/memory/physical.rs:259, 276` — buddy allocator freelist walks — **NEW**
- **Severity:** LOW (only manifests under memory corruption — double-free, stack stomp, driver bug)
- **Pattern:** `while let Some(block_ptr) = current { current = block.next; }` — no cycle detection. If the freelist is ever corrupt (e.g. self-referential `next`), every alloc/free freezes the kernel.
- **Sites:** `is_block_free` (line 259) and `remove_from_free_list` (line 276)
- **Fix:** 1M-hop cap with serial warning. On a 4 GB machine longest legitimate order-0 freelist is ~1M; exceeding it = corrupt = fail closed.

### 14. `kernel/src/arch/x86_64/smp.rs:155` — AP worker spin
- **Severity:** ⚠️ INEFFICIENT, not unsafe.
- **Pattern:** `while WORK_READY[cpu_index].load(...) == 0 { spin_loop(); }` — no HLT yield. Burns AP CPU at 100% during idle. Comment says "PAUSE-based spin wait... reducing power" — true for the CPU itself, but on WHPX/KVM each vCPU is a host thread so this still steals cycles.
- **NOT FIXED:** changing the wakeup model from spin to HLT+IPI would touch the parallel-GEMM dispatch path. Different scope.

## Verified safe in Pass 3 (full inventory)

**Kernel domain functions** — every loop has a bound or HLT yield:
- `task/scheduler.rs:423` — idle scheduler (HLT)
- `arch/x86_64/syscall/handlers/{io.rs:81, task.rs:124}` — poweroff/exit (HLT, intentional)
- `arch/x86_64/idt.rs:153/163/175` — fault halt loops (intentional)
- `arch/x86_64/smp.rs:85/260` — AP-ready waits (100M and 500M iter caps)
- `arch/x86_64/acpi.rs:296` — page-mapping loop (bounded by size)
- `drivers/keyboard.rs:377/390` — `read_key_blocking` (HLT yield)
- `drivers/mouse.rs:111/121` — PS/2 wait (100K iter cap)
- `drivers/nvme.rs:185, 764, 1260` — CAS retry, completion poll (caps + HLT), wait_ready (2M cap)
- `drivers/virtio_blk.rs:577, 681, 845` — I/O waits (50K, 500K, 100K caps)
- `drivers/virtio.rs:277` — descriptor-chain follow (bounded by queue size)
- `drivers/virtio_gpu/{commands.rs:215, flush.rs:253}` — bounded
- `drivers/cmos.rs:224` — year-from-epoch (~150 iters max)
- `drivers/telemetry.rs:224` — bounded by `max_count`
- `drivers/msix.rs:170` — bit-allocator CAS (returns None on exhaustion)
- `drivers/iommu.rs` — accessor functions (no loops)

**Userspace tasks** — all confirmed safe:
- `compositor/src/main.rs:86, 696` — IPC + main event loop
- `intent-service/src/main.rs:185` — IPC event loop
- `intent-bus/src/main.rs:49` — async channel rx
- `synapse-service/src/main.rs:131` — IPC event loop
- `synapse-service/src/btree.rs:1117` — BFS graph walk (depth+visited bounded)
- `shell/src/main.rs:30/34` — IPC drain loops (yield)
- `inference-server/src/handlers.rs:659` — tool-result wait (500K cap)
- `inference-server/src/bin/main.rs:66, 163, 226` — yield loops (intentional idle)
- `libfolk/src/sys/{io.rs:90, task.rs:13}` — failsafe post-syscall hangs (exit/poweroff never return; loop is correct)
- `libtensor/src/{arena.rs:61, tokenizer.rs:365/573/952}` — CAS/algorithmic, bounded
- `libsqlite/src/{varint.rs:78, shadow.rs:223, btree.rs:40}` — algorithmic, bounded by data size
- `compositor/src/mcp_handler/{knowledge_hunt.rs:368, agent_planner.rs:341}` — retry loops with `attempt < MAX_RETRIES` bound

**Total verified-safe loop sites:** 30+. The `loop`/`while let Some` pattern is used pervasively but ALL of them either have explicit caps, finite iteration bounds, HLT yields, or yield_cpu yields — except the 14 sites flagged across all three passes.

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
