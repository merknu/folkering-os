# Folkering OS WASM Drivers

This directory holds the source for every WASM driver bundled with
compositor as a fallback for hardware that doesn't have a driver in
Synapse VFS yet.

## Layout

```
drivers/
├── e1000_bootstrap.rs   →  e1000_bootstrap_v1.wasm   (8086:100E v1)
├── e1000_v2.rs          →  e1000_v2.wasm             (8086:100E v2, DMA)
├── virtio_net_v1.rs     →  virtio_net_v1.wasm        (1AF4:1000 legacy)
└── README.md            (this file)
```

Each `.rs` file is a standalone `#![no_std]` `#![no_main]` Rust program
that imports host functions via `extern "C"` and exports a single
`#[no_mangle] pub extern "C" fn driver_main()` entry point.

## Building

Two equivalent ways — pick whichever fits your flow:

1. **Automatic** — just build compositor. `userspace/compositor/build.rs`
   regenerates any out-of-date wasms before compiling:
   ```sh
   (cd userspace && cargo build -p compositor)
   ```

2. **Manual** — invoke the build script directly when you want fresh
   wasms without going through Cargo:
   ```sh
   tools/build-drivers.sh           # build all
   tools/build-drivers.sh --check   # CI: fail if any are stale
   ```

Both call `rustc` with the same byte-deterministic flags:

```
--target wasm32-unknown-unknown --edition 2021 --crate-type cdylib
-C opt-level=z -C strip=symbols
-C link-arg=--no-entry -C link-arg=--strip-all
```

## Why are the wasms not committed?

Earlier they were. That created a footgun: the committed `.wasm` could
silently drift from its `.rs` source after every rebuild, and `git status`
would constantly show them as modified. Now they live in `.gitignore` and
get regenerated from source on demand — there's exactly one source of
truth (the `.rs` file).

## Adding a new driver

1. Add `drivers/<name>.rs` following the structure of the existing files
   (no_std + no_main + panic_handler + a `driver_main` export).
2. Add `(<name>.rs, <name>.wasm)` to the `DRIVERS` table in
   `userspace/compositor/build.rs` and the `OUT_NAMES` map in
   `tools/build-drivers.sh`.
3. Wire it into `compositor/src/driver_runtime.rs`'s `BOOTSTRAP_DRIVERS`
   table with the right PCI vendor/device ID.

## Host-function ABI

Drivers call host functions exposed by compositor's WASM runtime
(`userspace/compositor/src/driver_runtime.rs` and `host_api/`). Function
signatures are declared `extern "C"` in each driver — keep them in sync
with the host side. See `e1000_v2.rs` for the most complete imports list.
