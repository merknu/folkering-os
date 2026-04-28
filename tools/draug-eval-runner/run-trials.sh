#!/usr/bin/env bash
# Run N trials of `eval --all` for each of the two prompt conditions
# (with CodeGraph caller list, without). Each run lands in its own
# output dir so post-hoc aggregation can read everything at once.
#
# Designed to be idempotent: if a trial dir already has score.json
# files for all 5 tasks, skip it. Useful when the LLM hangs and we
# need to restart without reburning successful trials.
#
# Usage:
#   tools/draug-eval-runner/run-trials.sh [N]   # N defaults to 3
#
# Estimated runtime: ~5 min per condition × N runs × 2 conditions.
# At N=3 expect ~30 min wall clock.

set -uo pipefail

N="${1:-3}"
ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
BIN="$ROOT/tools/draug-eval-runner/target/release/draug-eval.exe"

if [[ ! -x "$BIN" ]]; then
    echo "ERROR: draug-eval binary not found at $BIN" >&2
    echo "       run \`cd tools/draug-eval-runner && cargo build --release\` first" >&2
    exit 2
fi

cd "$ROOT"

ALL_TASKS=(01_pop_i32_slot 02_maybe_bounds_check 03_alloc_pages 04_compile_module 05_push_dec)

trial_complete() {
    local dir="$1"
    for task in "${ALL_TASKS[@]}"; do
        if [[ ! -f "$dir/$task/score.json" ]]; then
            return 1
        fi
    done
    return 0
}

run_trial() {
    local label="$1"      # cg or nocg
    local extra_flag="$2" # --no-codegraph or empty
    local i="$3"
    local out="tools/draug-eval-runner/output-${label}-r${i}"

    echo
    echo "=========================================================="
    echo "  trial: $label run $i  →  $out"
    echo "=========================================================="

    if trial_complete "$out"; then
        echo "[run-trials] $out already complete; skipping"
        return 0
    fi

    if [[ -n "$extra_flag" ]]; then
        "$BIN" "$extra_flag" --output "$out" eval --all
    else
        "$BIN" --output "$out" eval --all
    fi
}

for i in $(seq 1 "$N"); do
    run_trial "cg"   ""               "$i"
    run_trial "nocg" "--no-codegraph" "$i"
done

echo
echo "[run-trials] all $N × 2 trials done"
echo "[run-trials] aggregate with: python tools/draug-eval-runner/aggregate.py \\"
echo "  tools/draug-eval-runner/output-cg-r* tools/draug-eval-runner/output-nocg-r*"
