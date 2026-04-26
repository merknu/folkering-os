# Folkering CodeGraph spike — results

**Status:** Day 1 H1-4 done; H5-6 (Draug integration) pending.
**Started:** 2026-04-26
**Ended:** _to be filled in_
**Total hours actually spent:** ~1 (H1-2: builder + tests + smoke + verify) — well under budget so far.

---

## Day 1 H1-4 findings (preliminary)

### Builder works on full Folkering monorepo

| Metric | Value |
|---|---:|
| Vertices (functions discovered) | **4,762** |
| Edges (over-approximated) | 153,466 |
| CSR bytes (row_offsets + col_indices) | **618 KB** |
| Builder wall time (cold, release build) | 3.9 s |
| Files syn failed to parse | _to be measured_ |

⚠️ **618 KB exceeds the 500 KB threshold from the kill matrix.**
The edge count is inflated by RTA-style over-approximation: any
`fn new` call resolves to *every* `fn new` in the codebase.
Plausible reductions:
  * Type-aware resolution would cut edges 5–10× (project-wide
    `new`/`default`/`from` are the worst offenders)
  * Even crude "prefer same-file simple-name match" heuristic
    would help

This is the spike's first real finding: **edge count, not vertex
count, is the memory bottleneck — and it's an over-approximation
artifact, not a fundamental limit.**

### Q1 sanity check (pop_i32_slot in a64-encoder)

| Source | Distinct callers |
|---|---|
| `grep -rln "pop_i32_slot\b"` (excluding fn def line) | 9 files |
| CSR `query-callers` | 8 files |
| Hand-verified ground truth | 8 files |

Discrepancy explained: stack.rs grep-hit was a `debug_assert_eq!`
**string literal** mentioning the name, not a call. CSR correctly
rejected it. Semantic signal beat textual match.

### Q2 sanity check (maybe_bounds_check)

| Source | Distinct callers |
|---|---|
| `grep -n "maybe_bounds_check("` (excluding fn def + doc) | 10 |
| CSR `query-callers` | 10 |

**Exact match.** mod.rs grep-hit was a doc comment; correctly
excluded by CSR.

### Lookup latency (H5-6: serialization landed)

Day 1's H5-6 chose path C from the spike-charter follow-up: add
`dump-graph` and `query-callers --load` so the build cost is paid
once and per-query latency is measured cleanly. Result:

| Phase | Time |
|---|---:|
| Build CSR + serialize to disk (one-time) | 730 ms (warm) |
| FCG1 blob on disk | 909 KB |
| Load blob into memory (`--load`) | **1 ms** |
| Lookup `pop_i32_slot` (29 callers) | **138 µs** |
| Lookup `maybe_bounds_check` (10 callers) | **149 µs** |
| End-to-end load + lookup | **~1.15 ms** |

Compared to a *conservative* LLM-Gateway baseline (200–500 ms per
"find callers of X" query, which is what we'd typically see for
Draug's source-reading approach), this is **~150–450× faster**.
Day 2 will measure the actual LLM-Gateway baseline, but the
preliminary signal is well past the 10× threshold.

The 138 µs lookup is forward CSR scan (`O(V + E)`). A CSC-based
reverse lookup would drop it to `O(d̄_in)` ≈ low microseconds, but
the spike scope explicitly skips CSC. Even with the linear scan,
we're nowhere near the budget.

---

## Hypothesis (from spike charter)

> A static CSR-based call-graph + one new host function lets Draug
> answer "what calls X?" at least 10× faster than the current
> LLM-Gateway-mediated approach, with ≥ same correctness, and
> < 500 KB memory for the full Folkering codebase.

## Decision matrix (committed before any measurement)

| Outcome | Action |
|---|---|
| ≥ 10× speedup on 4-of-5 queries AND correctness ≥ baseline AND memory < 500 KB | **Expand** — proceed to indirect calls, edge types, integrate into Liquid Apps work |
| 2–10× speedup but marginal | **Hold** — keep CSR as ad-hoc shell tool, do not expand to full subsystem |
| < 2× speedup OR correctness regression OR > 2 MB memory | **Kill** — document why and walk away |
| Draug's bottleneck is NOT call-graph lookup (e.g. it's LLM tokenisation) | **Kill, but learn** — note what the actual bottleneck is and address that instead |

---

## Day 2 H1-2: pre-committed test queries + CSR results

Queries chosen and ground truth established **before** any LLM-Gateway
baseline timings are run. Cherry-picking after the fact would invalidate
the spike — these results stand whether the LLM measurements come back
favourable or not.

### Q1 — `pop_i32_slot` callers (file granularity)

**Ground truth** (grep, post-filtered to remove fn def + doc lines):

```
tools/a64-encoder/src/wasm_lower/call.rs
tools/a64-encoder/src/wasm_lower/control.rs
tools/a64-encoder/src/wasm_lower/convert.rs
tools/a64-encoder/src/wasm_lower/globals.rs
tools/a64-encoder/src/wasm_lower/memory.rs
tools/a64-encoder/src/wasm_lower/mod.rs
tools/a64-encoder/src/wasm_lower/scalar.rs
tools/a64-encoder/src/wasm_lower/simd.rs
```
**8 distinct files.**

**CSR result:** 8 distinct files. **Exact match.** Lookup: 138 µs.

### Q2 — `maybe_bounds_check` callers (file granularity)

**Ground truth (grep, filtered):**
```
tools/a64-encoder/src/wasm_lower/memory.rs
tools/a64-encoder/src/wasm_lower/simd.rs
```
**2 distinct files.**

**CSR result:** 2 distinct files. **Exact match.** Lookup: 149 µs.

### Q3 — `lower_op` callers (file granularity)

**Ground truth (grep, filtered):**
```
tools/a64-encoder/src/wasm_lower/mod.rs
tools/a64-encoder/src/wasm_lower/tests.rs
```
**2 distinct files.**

**CSR result:** 2 distinct files. **Exact match.**

### Q4 — Functions that call BOTH `push_i32_slot` AND `pop_i32_slot`

This is a set-intersection query. **Grep cannot do this directly** —
it's line-oriented, not function-scoped. CSR makes it trivial.

**CSR result:** 9 functions:
```
Lowerer::lower_call
Lowerer::lower_call_indirect
Lowerer::lower_call_internal
Lowerer::lower_i32_extend_narrow
Lowerer::lower_load
Lowerer::lower_op
Lowerer::lower_binop
Lowerer::lower_eqz
Lowerer::lower_select
```

Spot-checked against the source — every entry has both calls in its
body. Manual ground truth via `awk` over function-scoped chunks would
work but is tedious; CSR is the natural query medium for this shape.

### Q5 — Dead code (zero in-degree)

**Caveat acknowledged in spike charter:** v0 doesn't model macro-
generated calls, trait-object dispatch, or `#[test]` invocation by
the test harness. So "dead code" output includes legitimate roots.

**CSR result:** 1,169 unreferenced of 4,762 vertices (24.5%). Lookup: 85 µs.

A 24.5% noise floor is too high to be useful as-is. To make this query
actionable, post-spike work would need:
  * Filter out `#[test]` fns (cargo test invokes them)
  * Filter out `pub fn` exposed at crate boundaries
  * Filter out `extern "C"` and `#[no_mangle]`
  * Track trait-object dispatch (RTA-style: any `dyn Trait` use)

For the spike's purposes, Q5 demonstrates the API works but is the
weakest of the five queries. We'd document it as a known limitation
of v0 in any expand-decision.

---

## Measurements (Day 2 H3-4 — fill in after running)

| Query | LLM-Gateway time | CSR time | Speedup | LLM correct? | CSR correct? |
|---|---:|---:|---:|---|---|
| Q1 | _ ms | _ ms | _× | ✓/✗ | ✓/✗ |
| Q2 | _ ms | _ ms | _× | ✓/✗ | ✓/✗ |
| Q3 | _ ms | _ ms | _× | ✓/✗ | ✓/✗ |
| Q4 | _ ms | _ ms | _× | ✓/✗ | ✓/✗ |
| Q5 | _ ms | _ ms | _× | ✓/✗ | ✓/✗ |

| Metric | Value |
|---|---:|
| Total Folkering codebase: vertices | _ |
| Total Folkering codebase: edges | _ |
| CSR bytes (full codebase) | _ KB |
| Builder wall time (cold) | _ s |
| Token cost per LLM-Gateway query | _ tokens |
| Token cost per CSR query | 0 (no LLM call) |

---

## Decision (Day 2 H5-6 — fill in last)

**Outcome:** _Expand / Hold / Kill / Kill-but-learn_

**Rationale (3-5 sentences):**
_to be filled in_

**If "Kill":** what was the actual bottleneck?
_to be filled in_

**If "Expand":** what's the smallest next step that adds real value?
_to be filled in_

**If "Hold":** what triggers re-evaluation?
_to be filled in_

---

## Honest post-mortem (regardless of outcome)

**What surprised me?**
_to be filled in_

**What did I underestimate?**
_to be filled in_

**What would I do differently next time?**
_to be filled in_

**Did I respect the 12-hour time-box?** _Yes / No / By how much?_
_to be filled in_
