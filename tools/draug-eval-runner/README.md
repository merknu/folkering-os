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

## What it will do tomorrow (Phase 2, lands with step 3)

Each task additionally gets fed to Draug. The resulting patch is applied
to a sandbox copy of the monorepo, `cargo check` runs on the target file
+ every caller file, and the score is reported. **Compile + caller-compat
is the headline metric** — that's what CodeGraph integration is supposed
to enable, so that's what gets measured.

## Running

```sh
cargo run -p draug-eval-runner --release
# or
tools/draug-eval-runner/target/release/draug-eval
```

```
[draug-eval] 5 task(s) loaded from tools/draug-eval-runner/tasks.toml
[draug-eval] building CSR from . ...
[draug-eval] CSR ready (4835 vertices, 95902 edges, 402952 bytes) in 762 ms

[PASS] 01_pop_i32_slot (29 callers across 8 files)
[PASS] 02_maybe_bounds_check (10 callers across 2 files)
[PASS] 03_alloc_pages (4 callers across 1 files)
[PASS] 04_compile_module (5 callers across 4 files)
[PASS] 05_push_dec (12 callers across 1 files)

[draug-eval] summary: 5 passed, 0 failed
```

Exit code: `0` on full pass, `1` on any task fail, `2` on infrastructure
error (bad fixture, can't build CSR, etc).

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
