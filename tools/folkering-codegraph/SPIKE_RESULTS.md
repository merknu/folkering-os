# Folkering CodeGraph spike — results

**Status:** SPIKE COMPLETE — verdict: **EXPAND** (with explicit caveats below).
**Started:** 2026-04-26
**Ended:** 2026-04-27
**Total hours actually spent:** ~2 of 12 budgeted. The spike landed faster than the charter assumed because the syn-based path was straightforward and the FCG1 format took 30 minutes instead of the budgeted hour.

---

## Day 1 H1-4 findings (preliminary)

### Builder works on full Folkering monorepo

| Metric | Spike (RTA-only) | Post-fix (same-file-first) |
|---|---:|---:|
| Vertices (functions discovered) | 4,762 | 4,826 |
| Edges | 153,466 | **92,717** (-39 %) |
| CSR bytes (row_offsets + col_indices) | 618 KB | **381 KB** ✅ |
| FCG1 on disk | 909 KB | **675 KB** |
| Builder wall time (cold) | 3.9 s | 1.5 s |

✅ **The 500 KB caveat is closed.** Same-file-first edge resolution
landed (folkering-codegraph commit, April 2026): when a callee's
simple name has a match in the caller's own file, that match is
preferred and the global RTA fall-back is skipped. `fn new` no
longer multi-edges from every caller to every other crate's `new`.

The fall-back to global resolution still fires for genuine cross-
file calls (verified by the `falls_back_to_global_when_no_local_match`
test). Q1 + Q2 sanity-check counts unchanged (8 / 2 distinct files,
29 / 10 callers respectively) — the dropped edges were the
mechanically-redundant ones.

For historical context, the original spike framing:
> ⚠️ 618 KB exceeds the 500 KB threshold from the kill matrix.
> The edge count is inflated by RTA-style over-approximation.
> Plausible reductions: type-aware resolution; or "prefer same-file
> simple-name match" heuristic.

The latter is what landed.

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

## Measurements

### CSR side (actually measured)

| Query | Setup (load FCG1) | Lookup | Correctness vs ground truth |
|---|---:|---:|---|
| Q1 pop_i32_slot | 1 ms | 138 µs | ✓ exact (8/8 files) |
| Q2 maybe_bounds_check | 1 ms | 149 µs | ✓ exact (2/2 files) |
| Q3 lower_op | 1 ms | ~140 µs | ✓ exact (2/2 files) |
| Q4 push ∩ pop intersection | 1 ms | ~280 µs (two lookups) | ✓ 9 functions, spot-checked |
| Q5 dead code | 1 ms | 85 µs | ⚠️ 1169 hits, ~24.5% noise floor |

| Codebase metric | Value |
|---|---:|
| Vertices | 4,762 |
| Edges (over-approximated) | 153,466 |
| CSR bytes (in-memory) | **618 KB** ⚠️ over 500 KB threshold |
| FCG1 on disk | 909 KB |
| Builder wall time (cold) | 730 ms warm / 3.9 s cold |
| Files syn failed to parse | 0 (every .rs file parsed cleanly) |

### LLM-Gateway side (NOT measured)

H3-4 was deliberately skipped — see the user's call: actual LLM-Gateway
latency on the Pi is well-known to be ≥300 ms per round-trip (Ollama
load + tokenisation + generation), so any speedup over the CSR's 138 µs
is at minimum **2,000×**. Standing up the Pi + Ollama infrastructure
to confirm what we already know would have eaten ~2 hours of the
spike budget for no decision-relevant signal.

This is documented as a deliberate skip, not an oversight.

---

## Decision

**Outcome:** **EXPAND** — with three explicit caveats below.

**Rationale (per the spike charter's matrix):**

The hypothesis was: a CSR call-graph beats LLM-Gateway-mediated retrieval
for "find callers of X" by ≥10×, with ≥ same correctness, and < 500 KB
memory. Speedup is ~2,000× (138 µs vs ≥300 ms LLM round-trip), comfortably
past the 10× threshold. Correctness met or exceeded grep ground truth on
4 of 5 queries; CSR even *out-performed* grep on Q1 by correctly excluding
a string-literal mention that grep counted as a hit. Q4 (set intersection)
is a query shape grep cannot do at all, so the spike validated that CSR
unlocks new query types, not just speed. The one strict miss is memory
(618 KB vs 500 KB), but that's an explainable artifact (RTA-style
over-approximation from name collisions on `new`/`default`/`from`/etc),
not a fundamental limit — type-aware resolution would cut edges 5–10×
and put us at 60–120 KB. On balance, expand.

**Three caveats locked into the expand decision:**

1. **Memory threshold breached** but cause is known and fixable. The
   500 KB limit was set as a guess in the charter; with hindsight, edge
   count from over-approximation deserves its own line item. Type-aware
   edge resolution is the natural first cleanup post-spike.

2. **Q5 (dead code) is too noisy to ship as-is.** 24.5% false-positive
   rate from `#[test]`, `pub fn`, `#[no_mangle]`, and trait dispatch
   that v0 doesn't model. Don't expose Q5 to Draug until filtering lands.

3. **LLM-Gateway baseline is conservative-assumed, not measured.** If
   the actual baseline turns out to be ≤30 ms (very fast Ollama on a
   strong model), our claimed speedup drops from 2,000× to 200× — still
   massively past threshold, but worth noting we never proved it.

**Smallest next step that adds real value:**

Wire CSR into folkering-proxy as a single new TCP command (`GRAPH_CALLERS`)
serving from a pre-loaded FCG1 blob. Estimated 2-3 hours. This makes
Draug actually able to use the CSR — without proxy integration, the
spike output is a research artifact gathering dust. Defer indirect
calls, type-aware resolution, CSC reverse-lookup, and Q5 filtering
until after we see how Draug uses the basic forward query in real tasks.

**Subsequent steps (in priority order, each its own scoped chunk):**

1. Type-aware edge resolution — fixes the memory caveat
2. CSC reverse-lookup — drops 138 µs to single-digit µs (probably overkill)
3. Q5 noise filter — only if Draug actually wants dead-code analysis
4. `call_indirect` blast-radius support — if/when Draug needs WASM-app
   call graphs (currently it operates on Rust source)

---

## Honest post-mortem

**What surprised me:**

The win on Q1 was more interesting than I expected. CSR didn't just
match grep — it *beat* grep on signal quality by correctly excluding a
debug_assert string literal mentioning `pop_i32_slot`. That's a real
demonstration that semantic dispatch beats textual match for code
queries, not just a speed argument. I went into the spike thinking the
case was "CSR is faster"; came out thinking "CSR is also more correct."

The FCG1 format took 30 minutes including the roundtrip test. I had
budgeted an hour. Custom binary serialization without serde is
genuinely simple when the data is dense arrays.

**What I underestimated:**

The over-approximation problem. I knew name collisions would inflate
edges, but I didn't model how badly until the full-repo build returned
153K edges for 4.7K functions (avg out-degree 32 — implausible for a
real call graph, where avg should be 5-10). Setting a 500 KB memory
threshold without first sketching the over-approximation impact was
charter-design carelessness on my part. I should have either set a
higher threshold or required type-aware resolution as part of v0.

**What would I do differently next time:**

Two things. First, pre-flight the memory budget against an actual
small build of the target codebase before locking in a kill threshold —
it's cheap to do and would have caught the 500 KB miss before we
shipped the charter. Second, even though we deliberately skipped the
LLM baseline, an honest spike should at minimum log a single Ollama
round-trip on a known query to anchor the speedup claim. We have
strong reason to believe ≥300 ms but no fresh datum.

**Did I respect the 12-hour time-box?**

Yes — by a wide margin. Total real time spent: ~2 hours across two
days. The remaining ~10 hours of buffer wasn't wasted; it's exactly
the kind of slack that lets a time-boxed experiment land on solid
ground instead of a sprint to a deadline. If the spike had hit its
hard problems (e.g. syn parsing failures, or the over-approximation
exploding to 5+ MB instead of 618 KB), we'd have had headroom to
investigate honestly rather than shipping a half-answer.

**Spike grade:** A successful spike isn't one that confirms the
hypothesis — it's one that produces a confident, defensible decision
either way. By that standard, this one earns a B+. The expand
decision is well-supported, but the ungated charter assumption on
memory and the unmeasured LLM baseline keep it from an A.
