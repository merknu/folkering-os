# Draug Refactor-Flow Eval Runner

Pre-committed evaluation harness for Draug's autonomous refactor pipeline.

## Why this exists

Step 3 of the post-CodeGraph plan is teaching Draug to use the call-graph
when refactoring existing functions: query callers → fold blast-radius
into the prompt → patch → verify callers still compile. Before we measure
whether that integration improves Draug's outputs, we lock in the test
set. Adding tasks **after** measurement starts is cherry-picking and
makes the result unfalsifiable.

This crate is that lock-in.

## What it does today (Phase 1)

Loads `tasks.toml`, builds a CodeGraph CSR over the monorepo, and verifies
every task's frozen caller count + caller-file set still matches reality.

If the CSR drifts away from the locked-in expectations, the runner fails
and you decide:
- The fixture is stale because the codebase legitimately changed → refresh it
- The graph regressed → fix CodeGraph

This is also a regression test for `folkering-codegraph` itself: the same
five functions yield the same caller answer commit after commit.

## Phase 2A: refactor flow infrastructure (live now)

Two new subcommands let you actually drive a refactor end-to-end:

* `prompt <task-id>` — assemble the LLM-facing refactor prompt and
  write it to `output/<id>/prompt.md`. No LLM call. Useful for
  inspecting the prompt before paying for tokens.

* `refactor <task-id>` — assemble the prompt, ship it to the host-side
  `folkering-proxy` LLM endpoint (default `127.0.0.1:14711`,
  `qwen2.5-coder:7b`), save the response, and pull the first `​```rust​`
  fenced block out as a `refactor.md`.

The prompt folds together three things:
  1. The original source extracted verbatim from the tree (layout
     preserved, comments included).
  2. The caller list from CodeGraph — Draug's blast radius.
  3. The refactor goal + a small constraint set (preserve signature
     unless authorized, output a single fenced block, no diff format).

## Phase 2B: applying + scoring (live now)

The `score` and `eval` subcommands close the loop: apply the LLM
refactor to a sandbox, run `cargo check`, write a JSON verdict.

**The sandbox** is a persistent git worktree at `sandbox/`. First
run creates it from HEAD; subsequent runs reset uncommitted changes
between tasks. Worktree means it shares `.git` with the main repo
(no source duplication) but has its own `target/` for cargo's
incremental cache. Cold first-build of the kernel workspace is ~45 s;
warm reruns are ~2 s.

**Patch application** picks one of two strategies based on the LLM's
output:
  - **Replace** — patch contains `fn <target_fn>` → splice into the
    original fn's byte range, preserving indent + surrounding code.
  - **Append** — patch defines other names (e.g. `alloc_pages_with_layout`
    alongside the original) → insert into the same impl block when
    the original lives in one, otherwise at end of file.

**Cargo args per workspace** — kernel + userspace are `#![no_std]`
so `--all-targets` would fail with E0463 "can't find crate for test".
Tool crates (a64-encoder etc) keep `--all-targets` to also catch
caller-breakage in `examples/`.

**Verdict** is `PASS` only when `cargo check` exits 0. The full
diagnostic set (errors + warnings + a stderr excerpt focused on the
diagnostic blocks themselves, not cargo's progress noise) lands in
`output/<id>/score.json` for downstream aggregation.

## Running

```sh
# Verify CSR matches frozen task fixtures (default, no LLM):
cargo run -p draug-eval-runner --release

# Build a prompt only:
cargo run -p draug-eval-runner --release -- prompt 03_alloc_pages

# Full refactor against the proxy (proxy must be running):
cargo run -p draug-eval-runner --release -- refactor 03_alloc_pages

# Score the existing refactor.md against cargo check (sandbox):
cargo run -p draug-eval-runner --release -- score 03_alloc_pages

# Refactor + score in one go:
cargo run -p draug-eval-runner --release -- eval 03_alloc_pages

# Run the full suite:
cargo run -p draug-eval-runner --release -- eval --all

# Ablation — same suite without the CodeGraph caller list in the prompt:
cargo run -p draug-eval-runner --release -- \
    --no-codegraph --output output-no-cg eval --all

# Use a different model:
cargo run -p draug-eval-runner --release -- \
    --model gemma4:31b-cloud refactor 01_pop_i32_slot

# N=3 trial: 3 runs per condition + aggregator (writes Markdown + CSV):
tools/draug-eval-runner/run-trials.sh 3
python tools/draug-eval-runner/aggregate.py \
    tools/draug-eval-runner/output-cg-r* \
    tools/draug-eval-runner/output-nocg-r* \
    --csv tools/draug-eval-runner/n3-trial.csv
```

`verify` output:

```
[verify] 5 task(s); CSR 4887 verts / 97566 edges / 409816 bytes
[PASS] 01_pop_i32_slot (29 callers across 8 files)
[PASS] 02_maybe_bounds_check (10 callers across 2 files)
[PASS] 03_alloc_pages (4 callers across 1 files)
[PASS] 04_compile_module (5 callers across 4 files)
[PASS] 05_push_dec (12 callers across 1 files)

[verify] summary: 5 passed, 0 failed
```

Exit code: `0` on full pass, `1` on any task fail, `2` on infrastructure
error (bad fixture, can't build CSR, can't reach proxy).

## The five tasks

| ID | Function | Callers | Files | What it stresses |
|---|---|---:|---:|---|
| `01_pop_i32_slot` | JIT stack pop | 29 | 8 | Wide blast radius |
| `02_maybe_bounds_check` | bounds-check elision | 10 | 2 | Mid blast, semantic refactor |
| `03_alloc_pages` | kernel buddy allocator | 4 | 1 | Precision (no `new` collisions) |
| `04_compile_module` | host JIT entry point | 5 | 4 | Cross-crate-ish (examples + lib) |
| `05_push_dec` | TCP shell formatting | 12 | 1 | Tight cluster, single file |

Each one is a defensible refactor target — not contrived. See the
`description` field in `tasks.toml` for the actual change a refactor flow
would propose.

## Adding a task

1. Pick a real fn currently in the tree. Aim for caller counts in the 4-15
   range — too few and there's nothing to measure, too many and the task is
   really five tasks in a trenchcoat.
2. Build a fresh CSR and capture the ground truth:
   ```sh
   tools/folkering-codegraph/target/release/dump-graph.exe . /tmp/g.fcg1
   tools/folkering-codegraph/target/release/query-callers.exe \
       --load /tmp/g.fcg1 <fn-name>
   ```
3. Append a `[[task]]` block to `tasks.toml` with the captured count + file
   set.
4. Re-run the runner. New task should pass.
5. Document why in the task's `description` — what's the refactor we'd want
   Draug to attempt? This is the brief that step 3's prompt builder will
   consume.

## Don't do this

- Don't add tasks after Draug has been wired to use CodeGraph. Lock-in is
  the point.
- Don't tweak `expected_caller_*` to make a task pass after a CodeGraph
  change — investigate whether the change is correct first.
- Don't pick easy tasks (4 callers all in the same file, fn is a one-liner).
  We learn nothing.
