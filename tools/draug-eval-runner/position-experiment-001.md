# Position experiment 001 — does callers-at-end rescue gemma4?

**Date:** 2026-04-28
**Hypothesis:** The cross-model trial showed gemma4:31b-cloud
performing -10.5 pp worse with the CodeGraph caller list in the
prompt. One explanation was **goal dilution** — the long caller
list crowds the goal text out of the model's effective attention.
Moving callers to the bottom of the prompt should keep goal +
constraints adjacent and rescue pass-rate.

**Verdict: hypothesis not supported. But the data tells us something
more important.**

## Per-condition pass-rates (gemma4:31b-cloud, N=3 each)

| Position | with-CG | no-CG | within-batch diff |
|---|:-:|:-:|:-:|
| top    (original)         | 7/15 (46.7%) | 8/14 (57.1%) | -10.4 pp |
| bottom (this experiment)  | 6/15 (40.0%) | 6/15 (40.0%) | **0.0 pp** |

Two things to notice:

1. **Bottom-position cg vs nocg gap collapsed to 0** — neither
   position is "better". With callers at the bottom, the prompts
   converge to the same pass rate.

2. **The no-CG number itself dropped** from 57% (top batch) to
   40% (bottom batch) **on identical prompts** — no-CG never had
   a caller list to position. **That's the smoking gun: cloud
   variance is bigger than the position effect.**

## Per-task

| Task | cg-top | cg-bottom | nocg-top | nocg-bottom |
|---|:-:|:-:|:-:|:-:|
| 01_pop_i32_slot       | 3/3 | 3/3 | 3/3 | 3/3 |
| 02_maybe_bounds_check | 0/3 | 0/3 | 0/3 | 0/3 |
| 03_alloc_pages        | 1/3 | 0/3 | **2/3** | **0/3** |
| 04_compile_module     | 0/3 | 0/3 | 0/2 | 0/3 |
| 05_push_dec           | 3/3 | 3/3 | 3/3 | 3/3 |

Task 03 is the entire show. nocg-top (2/3) → nocg-bottom (0/3) on
**identical prompts** is the cleanest evidence: cloud-routed gemma4
gives different answers across batches, regardless of prompt content.

## What this means for the cross-model headline

The PR #43 headline was:

> qwen2.5-coder:7b: +20 pp from CG.
> gemma4:31b-cloud: -10.5 pp from CG.

The **direction** of the qwen result is well-replicated (3 single-shot
pilots + N=3 trial all show ≥ +20 pp). The 7b was tested locally
where temperature is 0 by default, so seed-to-seed variance is small.

The **gemma4 -10.5 pp** number was N=3 in a single cloud session.
This experiment ran another N=3 cloud session of the same prompt
(no-CG) and got 40% instead of 57% — a 17 pp swing on **identical
inputs**. That makes any g4 effect smaller than 17 pp impossible
to distinguish from session noise at N=3.

So the honest revised reading:

- **For 7b: CG context helps**, +20 pp, well-replicated.
- **For gemma4: we don't know.** N=3 isn't enough to see past
  cloud variance. The original -10.5 pp could be entirely session
  noise.

## What we actually learned

This is a methodological finding more than a scientific one:

1. **Cloud-routed models need either fixed seed OR N≫3** to
   produce comparable measurements. The eval harness currently has
   neither knob.

2. **Goal dilution is not the explanation** for whatever happens
   on g4. Moving the callers to bottom didn't differentiate cg
   from nocg either way.

3. **The 7b vs g4 comparison from PR #43 needs the caveat**
   strengthened. "Bigger model collapses the CG advantage" is
   plausible from the data — but **gemma4 may not actually be
   harmed by CG**, just less helped. We can't tell from N=3.

4. **The infrastructure is fine**, the trial is robust, the
   aggregator does what it says. We have a measurement rig; we
   don't have enough power for finely-grained cloud claims yet.

## What would actually settle this

1. **N=10 gemma4** at top-position cg vs nocg, run in a single
   continuous batch so cloud session conditions are matched.
   ~1.5 hour wall.
2. **Set Ollama temperature=0** for both local and cloud routes
   in the proxy LLM endpoint, so seed-to-seed variance shrinks.
   This is a 5-line patch.
3. **Per-batch baseline run**: every position/prompt experiment
   should also include an unchanged baseline arm in the same
   batch. Cross-session comparisons aren't safe.

## Reproducibility

```sh
# Original cross-model trial (top-position):
tools/draug-eval-runner/run-trials.sh 3 gemma4:31b-cloud g4 top
# This experiment (bottom-position):
tools/draug-eval-runner/run-trials.sh 3 gemma4:31b-cloud g4end bottom
```

All 12 g4 trials are in `output-{cg,nocg}-{g4,g4end}-r{1,2,3}/`
with `model=gemma4:31b-cloud` and `callers_position={top,bottom}`
in their score.json so any future re-analysis can segment cleanly.
