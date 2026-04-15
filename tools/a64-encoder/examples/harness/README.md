# AArch64 JIT Execution Harness

Phase 3 test-rig for `a64-encoder`. Runs JIT-emitted AArch64 byte
sequences on real aarch64 hardware (Raspberry Pi 5 / Cortex-A76) and
reports the function's return value as the process exit code.

## Architecture

```
┌──────────────────────────────────┐      SSH stdin      ┌─────────────────────────────┐
│ Host (Windows / x86_64)          │ ─────────────────▶ │ Raspberry Pi 5 (aarch64)    │
│                                  │                     │                             │
│  cargo run --example run_on_pi   │                     │  ~/a64-harness/run_bytes    │
│    │                             │                     │    │                        │
│    ▼                             │                     │    ▼                        │
│  Lowerer → Vec<u8>               │                     │  mmap(RWX) + memcpy         │
│  (stack → register, WasmOp →     │                     │  __clear_cache              │
│   A64 instructions, branches     │                     │  fn_t fn = (fn_t)mem;       │
│   patched)                       │                     │  int rv = fn();             │
│    │                             │                     │  return rv & 0xFF;          │
│    └─ bytes via ssh stdin ──────┼────────────────────▶│                             │
│                                  │ ◀───── exit code ── │                             │
└──────────────────────────────────┘                     └─────────────────────────────┘
```

The host-side never sees an aarch64 CPU — it just emits bytes. The
Pi reads them on stdin, drops them into mapped-executable memory,
and calls them like any other function. Exit code flows back through
SSH, and the Rust runner in `../run_on_pi.rs` asserts it against the
expected value.

## Deploy on the Pi

On a fresh Raspberry Pi OS aarch64 install (Debian 13 trixie is what
we tested on), copy `run_bytes.c` over and compile once:

```sh
# From the host (adjust host/user):
scp run_bytes.c knut@folkering-daq.local:~/a64-harness/
ssh knut@folkering-daq.local \
    "gcc -O2 -Wall -no-pie -o ~/a64-harness/run_bytes ~/a64-harness/run_bytes.c"
```

`-no-pie` matters: with PIE (the Debian default) the helper functions
used by Phase 4A would be placed at a fresh ASLR-randomised address
on every `run_bytes` invocation.  The Phase 4A flow queries the
helper address in one SSH call and runs the JIT in a second SSH call
— PIE would land them on different layouts and the baked-in
MOVZ/MOVK address would point at garbage.  `-no-pie` pins the
addresses at link time so they're stable across invocations.

The binary is stand-alone (glibc + kernel syscalls only), ~70 KiB.
No root required — `mmap(PROT_EXEC)` is allowed in user processes on
a standard Pi OS kernel.

## Run a test from the host

```sh
cd tools/a64-encoder
cargo run --example run_on_pi                # Phase 3: 6 stack+arith+if/else cases
cargo run --example call_on_pi               # Phase 4A: BLR into a real C helper
cargo run --example run_on_pi -- user@host   # override destination
```

The `call_on_pi` example exercises the `Call(n)` lowering (MOVZ/MOVK
chain → X16, BLR X16) against one of the `helper_*` functions
compiled into `run_bytes`.  It first asks `run_bytes --addrs` for
the helper's current absolute address, then bakes that address into
the emitted JIT.  A successful run prints
`[ ok ] Call(helper_return_42) returned 42 via BLR`.

Expected output:

```
Running 6 cases on knut@192.168.68.72...

  [ ok ] return 42: got 42
  [ ok ] i32.const 10 + 20: got 30
  [ ok ] i32.const 100 - 58: got 42
  [ ok ] nested add: 1+2+3: got 6
  [ ok ] if-else truthy: cond=1 → 10: got 10
  [ ok ] if-else falsy: cond=0 → 20: got 20

6 passed, 0 failed
```

Each row verifies a different slice of the emitter:

| Case                  | Exercises                                              |
|-----------------------|--------------------------------------------------------|
| `return 42`           | `MOVZ X0, #42 ; RET` — AAPCS64 result register         |
| `10 + 20`             | ADD shifted-register + 2-slot operand stack (X0, X1)   |
| `100 - 58`            | SUB with left/right operand order preserved            |
| `1 + 2 + 3`           | Nested register renaming across chained binops         |
| `if-else truthy`      | CBZ forward-patch → else-branch start                  |
| `if-else falsy`       | B forward-patch → end-label, Else depth reset          |

## Safety notes

- `run_bytes` reads up to 64 KiB from stdin. Larger bytes would need
  a buffer bump — not needed for Phase 3.
- `mmap(PROT_EXEC)` on Linux enforces W^X at the CPU level. We write
  then call without flipping protection bits; that works because we
  requested RWX up front. A hardened deployment would split into two
  mmaps (RW for copy, then `mprotect(PROT_EXEC)` to lock).
- `__builtin___clear_cache(start, end)` is **mandatory** on aarch64
  after writing instructions: the D-cache and I-cache are not
  coherent by default, and skipping the flush means the CPU fetches
  stale bytes (often crashes, sometimes worse — silently wrong code).
- Exit code is the low 8 bits of `X0`. Tests pick expected values
  <256; for larger return values, print from inside the JIT'd code
  or extend the harness to read a longer result.
