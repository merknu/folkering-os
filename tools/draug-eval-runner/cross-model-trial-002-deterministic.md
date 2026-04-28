---
date: 2026-04-28
type: documentation
project: folkering-os
tags: [draug-eval, codegraph, determinism, gemma4, ollama]
related: ["[[cross-model-trial-001]]", "[[position-experiment-001]]"]
---

# Cross-model trial 002 — deterministic LLM re-validation

**Date:** 2026-04-28
**Goal:** Re-run the gemma4:31b-cloud half of trial 001 with the
proxy now sending `temperature=0` AND `seed=42` to Ollama. Trial 001
showed gemma4 lost 10.5 pp from the caller list (the headline "the
gap inverts" finding). This trial asks: is that a real model
behavior or session noise from the cloud-routed model?

**Sample size:** N=1 per condition (with the LLM made deterministic,
N=3 collapses to N=1 — three identical seeds produce three identical
prompts produce three identical outputs).

## Headline — the -10.5 pp signal does not survive

|                          | with-CG       | no-CG         | Δ           |
|--------------------------|:-------------:|:-------------:|:-----------:|
| Trial 001 (stochastic, N=3) | 7/15 (46.7%) | 8/14 (57.1%) | **−10.5 pp** |
| Trial 002 (deterministic, N=1) | 3/5 (60.0%) | 2/5 (40.0%) | **+20.0 pp** |

A 30 pp swing on the same model, same five tasks, same prompts.
The honest reading: trial 001 measured cloud session variance, not
a property of how gemma4 uses the caller list.

## Per-task — only `03_alloc_pages` ever moved

Across all eight runs (3 stochastic with-CG + 3 stochastic no-CG +
1 det with-CG + 1 det no-CG), four of five tasks gave the same
verdict regardless of CG context:

| Task                       | All runs (with/no CG) | CG matters? |
|---------------------------|-----------------------|:-----------:|
| `01_pop_i32_slot`          | always PASS           | no          |
| `02_maybe_bounds_check`    | always FAIL (6 errors)| no          |
| `04_compile_module`        | always FAIL (3 errors)| no          |
| `05_push_dec`              | always PASS           | no          |
| `03_alloc_pages`           | mixed                 | **maybe**   |

`03_alloc_pages` per-run:

| Run        | with-CG | no-CG |
|------------|:-------:|:-----:|
| stoch-r1   | PASS    | PASS  |
| stoch-r2   | FAIL    | FAIL  |
| stoch-r3   | FAIL    | PASS  |
| det        | PASS    | FAIL  |

So 03 passed 4 of 8 times overall, with no clean correlation to CG
presence. The deterministic run flipped the way trial 001 *didn't*
predict (CG=PASS, no-CG=FAIL — the opposite sign). With one
deterministic sample we cannot say CG helps gemma4 on 03 either —
we just know the sign is unstable.

## What this means for the `by-model` policy

The `--cg-policy by-model` heuristic (`always` for small models,
`never` for ≥13b or `:cloud`) was justified two ways in trial 001:

1. **Token economy** — large models have plenty of room, but the
   ~500 byte caller list still costs throughput on cloud-routed
   inference. ✔ Still valid.
2. **Effect on quality** — gemma4 was 10.5 pp worse with CG.
   ✘ Now retracted: trial 002 shows the gap was noise.

So the policy stands, but on weaker grounds. We are no longer
saying "the caller list hurts large models." We are saying "the
caller list does not measurably help large models on this task set,
so we save the tokens." If a future trial shows gemma4 needs CG for
a task it currently fails (`02`, `04`), we should flip the
heuristic.

## Determinism check

Two consecutive proxy calls with `temperature=0 + seed=42` on a
1340-byte prompt produced byte-identical responses (verified during
the seed=42 patch landing, PR #3). Without `seed=42`, the same
prompt gave 765 vs 814 byte responses — small but real divergence.
This is why trial 001 N=3 gave non-zero variance per cell even on
"deterministic" `temperature=0`: Ollama's default seed-per-request
behavior was leaking entropy into the eval.

## Limitations and what would change the verdict

- **N=1 deterministic.** Only true for *this* seed. A different
  fixed seed could change the trial 002 numbers wholesale. The
  argument here is not "deterministic shows CG helps" — it's
  "deterministic shows the trial 001 result was not stable."
- **Small task set (5).** One flippy task (`03`) dominates the
  numbers. We should expand the fixture set before claiming any
  property of gemma4's CG behavior.
- **Proxy/cloud rate-limiting.** Task 05 in `cg always` was
  interrupted by HTTP 429 from the cloud-routed gemma4 endpoint
  three times across ~10 minutes before succeeding. Cloud capacity
  affects reproducibility on the human-time-frame even when the
  prompt is identical.

## Files

- `output-cg-g4det-r1/` — gemma4 deterministic, with caller list
- `output-nocg-g4det-r1/` — gemma4 deterministic, no caller list
- `output-cg-g4-r{1,2,3}/`, `output-nocg-g4-r{1,2,3}/` — trial 001
  stochastic data (kept for comparison)
