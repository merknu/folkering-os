#!/usr/bin/env bash
# Build all WASM drivers from drivers/*.rs sources.
#
# Each .rs file is a no_std/no_main standalone Rust program with extern "C"
# host imports and a single #[no_mangle] pub extern "C" fn driver_main entry
# point. compositor's `include_bytes!` consumes the resulting .wasm files.
#
# Output is byte-deterministic with these flags (verified against the
# previously hand-built blobs):
#   --target wasm32-unknown-unknown
#   --edition 2021
#   --crate-type cdylib
#   -C opt-level=z
#   -C strip=symbols
#   -C link-arg=--no-entry
#   -C link-arg=--strip-all
#
# Usage:
#   tools/build-drivers.sh         # build all drivers
#   tools/build-drivers.sh --check # verify wasms exist + are newer than .rs
#
# Designed to run on Git Bash (Windows), Linux, and CI.

set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
DRV_DIR="$ROOT/drivers"

# .rs source → .wasm output mapping. Bootstrap is "_v1" because compositor
# pins to that filename for the legacy fallback driver; the v2 / v1 suffixes
# distinguish iterations of the same hardware target, not the source file.
declare -A OUT_NAMES=(
    [e1000_bootstrap.rs]=e1000_bootstrap_v1.wasm
    [e1000_v2.rs]=e1000_v2.wasm
    [virtio_net_v1.rs]=virtio_net_v1.wasm
)

RUSTC_FLAGS=(
    --target wasm32-unknown-unknown
    --edition 2021
    --crate-type cdylib
    -C opt-level=z
    -C strip=symbols
    -C link-arg=--no-entry
    -C link-arg=--strip-all
)

mode="build"
if [[ "${1:-}" == "--check" ]]; then mode="check"; fi

cd "$ROOT"

newer_than() {
    # Returns 0 if $1 exists and is newer than $2.
    [[ -f "$1" ]] || return 1
    [[ -f "$2" ]] || return 1
    [[ "$1" -nt "$2" ]]
}

failed=0
built=0
skipped=0

for src in "${!OUT_NAMES[@]}"; do
    out="${OUT_NAMES[$src]}"
    src_path="$DRV_DIR/$src"
    out_path="$DRV_DIR/$out"

    if [[ ! -f "$src_path" ]]; then
        echo "[build-drivers] WARN: missing source $src_path" >&2
        failed=1
        continue
    fi

    if [[ "$mode" == "check" ]]; then
        if newer_than "$out_path" "$src_path"; then
            echo "[build-drivers] OK    $out (up to date)"
        else
            echo "[build-drivers] STALE $out (rebuild needed)"
            failed=1
        fi
        continue
    fi

    # Skip rebuild if output is already newer than source (incremental).
    if newer_than "$out_path" "$src_path"; then
        size=$(stat -c%s "$out_path" 2>/dev/null || stat -f%z "$out_path")
        echo "[build-drivers] skip  $out ($size bytes, up to date)"
        skipped=$((skipped + 1))
        continue
    fi

    echo "[build-drivers] build $src -> $out"
    if ! rustc "${RUSTC_FLAGS[@]}" -o "$out_path" "$src_path"; then
        echo "[build-drivers] FAIL  $src" >&2
        failed=1
        continue
    fi

    size=$(stat -c%s "$out_path" 2>/dev/null || stat -f%z "$out_path")
    echo "[build-drivers] done  $out ($size bytes)"
    built=$((built + 1))
done

if [[ "$mode" == "check" ]]; then
    if [[ $failed -ne 0 ]]; then
        echo "[build-drivers] check FAILED — run tools/build-drivers.sh" >&2
        exit 1
    fi
    echo "[build-drivers] check OK"
    exit 0
fi

if [[ $failed -ne 0 ]]; then
    echo "[build-drivers] one or more drivers failed to build" >&2
    exit 1
fi

echo "[build-drivers] summary: built=$built skipped=$skipped"
