# Folkering CodeGraph spike — results

**Status:** PRE-MEASUREMENT (Day 1 not yet started)
**Started:** _to be filled in_
**Ended:** _to be filled in_
**Total hours actually spent:** _to be filled in_

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

## Pre-committed test queries (Day 2 H1-2 — fill in BEFORE running anything)

These five queries are committed to git **before any measurements are taken**. Cherry-picking after the fact is lying to ourselves.

### Q1 — `pop_i32_slot` callers
- **Expected callers (manually verified ground truth):**
  - _to be filled in_
- **Source:** `grep -rn "pop_i32_slot" tools/a64-encoder/src/ | grep -v "fn pop_i32_slot"`

### Q2 — `maybe_bounds_check` callers
- **Expected callers (manually verified ground truth):**
  - _to be filled in_
- **Source:** `grep -rn "maybe_bounds_check" tools/a64-encoder/src/`

### Q3 — `lower_op` callers
- **Expected callers (manually verified ground truth):**
  - _to be filled in_

### Q4 — Functions that call BOTH `push_i32_slot` AND `pop_i32_slot`
- **Expected (set intersection):**
  - _to be filled in_

### Q5 — Dead code (functions that nothing calls in the lowered codebase)
- **Expected (small list, possibly with public-API false positives):**
  - _to be filled in_
- **Note:** v0 won't see macro-generated calls, so this query has known noise.

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
