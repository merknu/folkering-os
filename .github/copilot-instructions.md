# GitHub Copilot Instructions for Folkering OS

When reviewing pull requests in this repository, act as a senior bare-metal Rust OS architect. Folkering OS is a `no_std`, AI-driven operating system that boots on KVM/WHPX/Proxmox and embeds asynchronous AI agents (Draug, Phase 17 refactor loop). Many bugs that look benign in normal Rust code are fatal here. Enforce these rules strictly.

## 1. Zero unbounded spin-loops (the "Tick #1 freeze" rule)

Never approve unbounded polling loops on hardware ports, MMIO, ring buffers, or syscalls.

- **Forbidden:** `while let Some(b) = port::read() { ... }`, `loop { if reg & FLAG == 0 { break } }`, `while !atomic.load() { spin }` — without an iteration cap or guaranteed `hlt()` yield.
- **Required:** every such site must have either (a) `for _ in 0..N` with a sane N, or (b) `core::arch::asm!("hlt")` inside the loop body so the scheduler sees the IRQ.

**Why this is fatal here:** QEMU/WHPX 16550 emulation can leave LSR DR=1 forever when COM3 has no backend. Bug #49 froze the compositor on iteration #1 of its main event loop because `god_mode::poll_com3` had `while let Some(byte) = com3_read()`. Same class of bug already showed up in `com3_write_byte` TX-empty wait, `keyboard.rs` PS/2 init drain, `cmos.rs` RTC update-in-progress wait, virtio_net `poll_rx`, and the firewall's denied-frame drain. Reference fix shape: `kernel/src/drivers/serial.rs::com2_async_poll` (cap 4096) or `kernel/src/drivers/nvme.rs::wait_ready` (cap + HLT).

When you see a new I/O loop, demand the cap. Off-by-one matters: prefer `>=` over `>` so `if drained >= 256 { break }` actually stops at 256, not 257.

## 2. Lock contention and serial logging

Holding a lock during serial output is a recipe for kernel-wide hangs.

- **Forbidden:** holding `NET_STATE`, `BLOCKLIST`, or any `spin::Mutex` while calling `crate::serial_str!` / `serial_println!`. Serial writes wrap in `interrupts::without_interrupts(...)` — combine that with a held lock and the timer ISR can't poll the network, which deadlocks Phase 17 outbound TCP.
- **Forbidden:** holding a synchronous `Mutex` across an `.await` point or `yield_cpu()`.
- **Required pattern:** read state into a local buffer, drop the lock, then log. For bulky census dumps under contention (e.g. socket-pool diagnostics), copy the small snapshot under the lock and emit serial outside it.

When `try_lock` is the right tool (timer ISR contexts), enforce a retry cap. `syscall_tcp_close` returning `EAGAIN` on first contention leaks slots — use the 1000-spin retry pattern from `tcp_plain.rs`.

## 3. Network resilience — smoltcp + VirtIO

The OS must survive SYN floods, broadcast storms, and host-side ARP weirdness.

- **Forbidden:** unbounded packet drains on RX. `kernel/src/drivers/virtio_net::poll_rx` and `kernel/src/net/device.rs::receive` must cap at ≤256 packets per call so other ISR-driven work makes progress under flood.
- **Forbidden:** permanent firewall blocklist entries. Auto-block must expire (Folkering uses 120s) so a host that floods us briefly doesn't get banned forever — including hosts we legitimately initiated TCP to (issue #58 root cause).
- **Forbidden:** sole reliance on TCP for liveness probes. If a code path's wakeup signal shares the wedged TCP state (e.g. `proxy_ping` calling `tcp_plain::tcp_request_with_timeout`), the wedge is permanent. Prefer UDP-based or ICMP-based independent probes for recovery paths.

## 4. Bare-metal Rust practices

- **Required:** `no_std` everywhere. No `std::` imports anywhere under `kernel/` or in `userspace/*/no_std`-marked crates.
- **Required:** `Send` markers must be justified — `unsafe impl Send` for raw-pointer-holding structs needs a comment explaining the ownership invariant.
- **Required:** memory allocations in async tasks must be properly dropped on every TIMEOUT/abort path. Phase 17's `draug_async` allocates `async_request`/`async_response` buffers — each abort path must reset them or the heap high-watermark grows unbounded under flood.
- **Preferred:** typestate patterns over runtime checks (e.g. socket states as types, not enum + match). Zero-cost abstractions over raw pointer arithmetic.

## 5. PR-review style

- Reference these rules by section number when flagging a violation (e.g., "Rule 1: this `while let Some(...)` lacks an iteration cap; mirror the 4096-cap in `com2_async_poll`").
- Be specific about the failure mode, not vague ("this could deadlock"). Cite which environment (KVM, WHPX, broadcast storm) triggers the bug.
- Suggest the smallest fix, including a concrete code snippet when possible.
- Do not approve PRs that add new I/O loops, lock-and-log paths, or permanent state without expiration. Defer to the maintainer.

## Reference: shape of a correct cap

```rust
// Bounded for-loop, never dequeues the cap+1th item.
for _ in 0..256 {
    let frame = match rx::receive_packet_inner(dev) {
        Some(p) => p,
        None => break,
    };
    handle(frame);
}
```

vs. the buggy shape:

```rust
// Off-by-one: 257 dequeued, 1 silently dropped.
let mut n = 0u32;
while let Some(frame) = rx::receive_packet_inner(dev) {
    n += 1;
    if n > 256 { break; }
    handle(frame);
}
```

When the cap is checked *after* dequeue + increment, the cap+1th item is consumed and lost. Always cap *before* mutating state, or use a bounded `for` loop.
