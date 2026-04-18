# Workspace state — 2026-04-18

This file documents the state of `ai-native-os` for anyone who pulls
the branch and wants to know what's committed vs in-flight. As of
commit `34d6d83` **HEAD builds cleanly from a fresh clone** — see
"What was just landed" below.

---

## What was just landed (resolves the long-standing build break)

Commit `34d6d83` `chore: land in-progress kernel work so HEAD builds
from a fresh clone` committed everything that was preventing
origin/ai-native-os from compiling on its own:

  * `kernel/src/fs/mod.rs` — the missing `pub mod mvfs;` declaration
  * `kernel/src/capability/{mod,types}.rs` — the `grant_*`
    capability functions referenced by the syscall handlers
  * All 9 syscall handlers under `kernel/src/arch/x86_64/syscall/handlers/`
  * IPC + memory + task additions (`ipc/*`, `memory/paging.rs`,
    `task/{elf,spawn}.rs`, `fs/ramdisk.rs`)
  * `kernel/vendor/embedded-tls/` — vendored TLS crate (93 files)
    referenced by `kernel/Cargo.toml` (`path = "vendor/embedded-tls"`)
    but never actually `git add`ed

Effect: `cargo check --target x86_64-unknown-none` from a clean
clone now succeeds. No more "fix this on your local machine first"
ritual.

---

## Session commits (JIT platform + bench infra)

| Commit | Summary |
|--------|---------|
| `34d6d83` | `chore: land in-progress kernel work so HEAD builds` (this fix) |
| `dac32ae` → `3cd5bd7` | persistent-worker daemon → reverted, see issue #25 |
| `e789785` | `perf(daemon+bench): TCP_NODELAY → 25x throughput` (10 → ~250 ops/sec) |
| `0ecabb0` | `feat(jit): built-in benchmark suite` |
| `249a802` | `feat(kernel): generic jit_run_wasm` |
| `ee72a34` | `feat(a64-encoder): single-head self-attention block on Pi` |
| `41bc57c` | `feat(a64-encoder): real Rust→WASM→JIT→ARM end-to-end ML inference` |
| `a998b45` | `fix(a64-encoder): mlp_memory_on_pi via a64-stream-daemon` |
| `cd3b74d` | `feat(a64-encoder): Gen-6 platform` |
| `e77dbaa` | `feat(kernel): integrate a64-encoder JIT compiler` |
| `7c8e667` | `feat(kernel): SYS_JIT_EXEC syscall + 'jit' shell command` |

Hardware-validated on folkering-daq (Pi 5, 192.168.68.72:7700):
200 encoder examples + 3 MLP variants + 13 daemon smoke tests + 1
transformer attention head + 100-iteration bench, all matching
reference computations.

---

## What's still in the working tree (62 entries, doesn't block build)

The remaining WIP belongs to other parallel workstreams. None of it
is required for `cargo check`. Listed here so future sessions know
what's pending.

### Modified — folk_browser refactor (5 files)
`apps/folk_browser/Cargo.toml`, `lib.rs`, plus deletions of
`{gif,jpeg,png,webp}.rs`. Image-codec extraction in progress.

### Modified — userspace plumbing (12 files)
`userspace/{compositor,libfolk,shell,synapse-service}/` —
multiple in-flight PRs that haven't settled.

### Modified — misc (5 files)
`boot/{files.db,iso_root/.../initrd.fpk}` (build artefacts),
`mcp/_await_config.json`, `tests/src/lib.rs`,
`tools/folk-pack/src/main.rs`, `wasm-apps/folk_browser.wasm`,
`wasm_cache/dream_budget.json`.

### Deleted — old screenshots (10 files)
`screenshots/cw-gif/frame-0[0-9]-*.png` — superseded.

### Untracked — in-progress source (8 items)
  * `drivers/e1000_bootstrap.rs`, `drivers/poll_rx_new.rs`
  * `mcp/_tensor_test.rs`
  * `tests/extended_stress.py`, `tests/test_driver_gen.py`
  * `tools/fbp-rs/`, `tools/inject_driver.py`
  * `userspace/shell/src/commands/mvfs.rs`

### Untracked — likely build artefacts (6 items)
  * `apps/weather_demo.wasm`
  * `boot/files.db.full`
  * `drivers/{e1000_bootstrap_v1,e1000_v2,virtio_net_v1}.wasm`
  * `wasm-apps/folk_browser_raw.wasm`

Candidates for `.gitignore` rules.

### Untracked — screenshots (10 items)
  * `screenshots/browser-tests/01-hackernews{,-with-gif-webp}.ppm`
  * `screenshots/proxmox-{ai-metrics,ai-self-metrics,bridge-dns,
    bridge-tls,dns-clean,network-monitor,network-widgets,tls-test}.png`

---

## Re-verifying what works

```sh
# Build the kernel (must succeed on fresh clone now)
cd kernel && cargo check --target x86_64-unknown-none

# a64-encoder unit tests (242 tests)
cd tools/a64-encoder && cargo test

# Hardware tests against the Pi:

# 1. Smoke test (HMAC + DATA + EXEC over TCP) — must pass 13/13
./tools/a64-streamer/target/release/a64-stream-smoke-test 192.168.68.72:7700

# 2. Real Rust→WASM→JIT→ARM attention head — must return 2239
cd tools/a64-encoder/examples/wasm-attention
RUSTFLAGS="-C link-arg=--no-entry -C link-arg=--export=attention" \
  cargo build --target wasm32-unknown-unknown --release
cd ../.. && PI_HOST=192.168.68.72:7700 \
  cargo run --release --example run_real_wasm_attention

# 3. Bench (100 iterations of attention) — should report ~250 ops/sec
PI_HOST=192.168.68.72:7700 cargo run --release --example bench_real_wasm
```

Pi-side daemon (must be running on 192.168.68.72:7700):

```sh
ssh knut@192.168.68.72 \
  'nohup ~/folkering-build/a64-streamer/target/release/a64-stream-daemon \
   0.0.0.0:7700 > ~/a64-daemon.log 2>&1 < /dev/null &'
```

## Open issues

  * **#25** — Persistent worker in daemon corrupts results after CODE
    re-install cycles. Implementation reverted; documented for follow-up.
