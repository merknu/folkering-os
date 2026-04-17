# Workspace state — 2026-04-17

This file documents the state of the working tree on branch
`ai-native-os` at the close of the JIT-platform session. It exists
because the tree contains 124 in-flight changes from parallel
workstreams that were intentionally NOT cleaned up — see
"Why this isn't a stash" below.

If you've just pulled the branch and `cargo check` complains about
missing modules, this file explains why.

---

## What was completed and pushed this session

| Commit | Summary |
|--------|---------|
| `249a802` | `feat(kernel): generic jit_run_wasm` — kernel JIT pipeline accepts arbitrary `.wasm` from FPK ramdisk; HMAC-signed CODE frames; legacy `jit <ip>` MLP demo preserved |
| `ee72a34` | `feat(a64-encoder): single-head self-attention transformer block on Pi` — real Rust→WASM→JIT→ARM scaled-dot-product attention; PyTorch bit-exact (checksum 2239) |
| `41bc57c` | `feat(a64-encoder): real Rust→WASM→JIT→ARM end-to-end ML inference` — first real Rust-compiled .wasm executed on Pi; `0xFC trunc_sat` parser support; `FunctionBody::local_types` |
| `a998b45` | `fix(a64-encoder): mlp_memory_on_pi uses a64-stream-daemon` — DATA-frame weight delivery; previously broken via SSH+run_bytes |
| `cd3b74d` | `feat(a64-encoder): Gen-6 platform` — modular architecture, typed validator, ML inference demos |
| `e77dbaa` | `feat(kernel): integrate a64-encoder JIT compiler` — bare-metal WASM→AArch64 cross-compilation |
| `7c8e667` | `feat(kernel): SYS_JIT_EXEC syscall + 'jit' shell command` |

All seven commits are on `origin/ai-native-os`. Hardware-validated on
folkering-daq (Pi 5, 192.168.68.72:7700): 200 encoder examples + 3 MLP
variants + 13 daemon smoke tests + 1 transformer attention head, all
matching reference computations.

See `~/.claude/projects/.../memory/folkering-jit-hardware-validated.md`
for the full hardware-validation log.

---

## Why this isn't a stash

When I tried `git stash --include-untracked` to clean the tree, the
kernel stopped compiling. Investigation:

  * Commit `1bbca04` ("MSI-X + NVMe + MVFS-on-NVMe") committed
    `kernel/src/fs/mvfs.rs` and a set of `capability::grant_*`
    functions and MVFS syscall handlers — **but the bridging
    `pub mod mvfs;` line in `kernel/src/fs/mod.rs` was never
    committed**. The fix is sitting in the working tree as a
    1-line modification.
  * `kernel/vendor/embedded-tls/` is a vendored crate referenced
    by tracked TLS code but the vendor directory itself was never
    `git add`ed.

So origin/ai-native-os HEAD doesn't build by itself. The tree fills
the gaps. Stashing the gaps breaks things. The tree state is the
"working" state; HEAD by itself is the "broken" state.

This is pre-existing across multiple sessions, not something I
introduced. Cleaning it up requires deciding what's a feature
commit vs glue commit vs throwaway artifact, which means
understanding the intent of each in-flight change.

---

## What's in the tree (categorised, no judgement)

### M (45) — Modified tracked files

Mostly the kernel-refactor + browser-refactor that's been in flight
across earlier sessions.

  * `apps/folk_browser/` — image-codec deletions + lib refactor
  * `kernel/src/arch/x86_64/syscall/handlers/` — audio, compute, dma,
    fs, io, memory, net, pci, task — adds new syscall variants
  * `kernel/src/capability/` — new `grant_*` privileges
  * `kernel/src/fs/`, `kernel/src/ipc/`, `kernel/src/memory/`,
    `kernel/src/task/` — kernel internals refactor
  * `userspace/compositor/`, `userspace/libfolk/`, `userspace/shell/`,
    `userspace/synapse-service/` — userspace plumbing
  * `tests/src/lib.rs`, `tools/folk-pack/src/main.rs` — tooling
  * `boot/files.db`, `boot/iso_root/boot/initrd.fpk` — build artifacts
    (re-packed during local boot work)
  * `mcp/_await_config.json` — MCP config tweak

### D (14) — Deleted tracked files

  * `apps/folk_browser/src/{gif,jpeg,png,webp}.rs` — image codecs
    moved or removed (browser refactor)
  * `screenshots/cw-gif/frame-0[0-9]-*.png` — superseded screenshots

### ?? (65) — Untracked

Build-required, must stay in the tree:

  * `kernel/vendor/embedded-tls/` — vendored TLS crate; tracked
    code in the kernel `use`s this. ~30+ files.

In-progress source files (sessions you'll want to finish):

  * `drivers/e1000_bootstrap.rs`, `drivers/poll_rx_new.rs` — driver
    rewrites
  * `mcp/_tensor_test.rs` — MCP tool draft
  * `tests/extended_stress.py`, `tests/test_driver_gen.py` — test
    harness drafts
  * `tools/fbp-rs/`, `tools/inject_driver.py` — tooling drafts
  * `userspace/shell/src/commands/mvfs.rs` — shell command draft

Build artefacts (probably belong in .gitignore):

  * `apps/weather_demo.wasm`
  * `boot/files.db.full`
  * `drivers/e1000_bootstrap_v1.wasm`, `drivers/e1000_v2.wasm`,
    `drivers/virtio_net_v1.wasm`
  * `wasm-apps/folk_browser_raw.wasm`

Screenshots (debatable whether to commit):

  * `screenshots/browser-tests/01-hackernews{,-with-gif-webp}.ppm`
  * `screenshots/proxmox-{ai-metrics,ai-self-metrics,bridge-dns,
    bridge-tls,dns-clean,network-monitor,network-widgets,tls-test}.png`

---

## Recommended order of operations when you pick this up

1. **Make HEAD buildable.** Land the tiny "glue" set: `pub mod mvfs;`
   in `fs/mod.rs`, the `pub mod` lines in `capability/mod.rs`, and
   add `kernel/vendor/embedded-tls/` to git (or to `.gitignore` if
   it's pulled from elsewhere). This converts a multi-day "broken
   on origin" state into a clean commit.

2. **Decide on artifacts.** `apps/weather_demo.wasm`, `boot/files.db.full`,
   `drivers/*.wasm`, `wasm-apps/folk_browser_raw.wasm` — pick ignore
   or commit, then add `.gitignore` rules.

3. **Resume the open workstreams** (browser refactor, syscall
   additions, drivers, MVFS shell command) one at a time and commit
   them as coherent units instead of letting them mingle.

4. **Take a fresh `git status` and confirm clean.**

---

## How to test what was completed this session

The JIT pipeline is hardware-validated. To re-verify:

```sh
# a64-encoder unit tests (242 tests)
cd tools/a64-encoder && cargo test

# Real Rust→WASM→JIT→ARM attention head on Pi
cd examples/wasm-attention
RUSTFLAGS="-C link-arg=--no-entry -C link-arg=--export=attention" \
  cargo build --target wasm32-unknown-unknown --release
cd ../.. && PI_HOST=192.168.68.72:7700 \
  cargo run --release --example run_real_wasm_attention
# expected: "Pi result: 2239 (expected 2239 from reference.py)"

# All 17 encoder examples + 3 MLP variants on Pi (~3 min)
for ex in bitops bounds call cmp conv f32 f32_full f64 fib i64 \
          i64_full indirect loop memory module run simd; do
  PI_HOST=knut@192.168.68.72 cargo run --release --quiet \
    --example ${ex}_on_pi 2>&1 | grep -cE "\[ ok \]"
done
```

Pi-side daemon (must be running on 192.168.68.72:7700):

```sh
ssh knut@192.168.68.72 \
  'nohup ~/folkering-build/a64-streamer/target/release/a64-stream-daemon \
   0.0.0.0:7700 > ~/a64-daemon.log 2>&1 &'
```
