# N=3 trial 001 — CodeGraph context A/B

**Date:** 2026-04-28
**Model:** qwen2.5-coder:7b (local Ollama)
**Runs:** 3 per condition (6 trials total, ~30 min wall)
**Aggregator:** `python tools/draug-eval-runner/aggregate.py`

## Headline

**With-CG: 7/15 (47%) vs no-CG: 4/15 (27%) — +20 pp.**

The single-shot ablation (baseline-001 vs baseline-002) showed
+20 pp on N=1. N=3 reproduces the same gap. Stronger signal.

## Per-task

| Task | with-CG | no-CG | Δpp | Reading |
|---|:-:|:-:|:-:|---|
| `01_pop_i32_slot` | 3/3 | 3/3 | 0 | Caller-insensitive: signature is obvious from the body. Both conditions trivially correct. |
| `02_maybe_bounds_check` | 0/3 | 0/3 | 0 | Always fails: the 7b model can't do this refactor regardless of context. Different model needed. |
| `03_alloc_pages` | 2/3 | 0/3 | +67 | **CG-dependent**: with the caller list, model picks `append` strategy and preserves original. Without, it tries `replace` and breaks 4 callers. |
| `04_compile_module` | 0/3 | 1/3 | -33 | Hardest task (4900-char patches). One no-CG run accidentally landed a working refactor — likely noise on a near-impossible task. |
| `05_push_dec` | 2/3 | 0/3 | +67 | **CG-dependent**: callers all live in same file, but explicit blast-radius prompt nudges the model toward signature preservation. |

## What actually held up

1. **+20 pp suite-level lift from CG.** Two single-shot data points
   showing the same gap, plus N=3 trial showing the same gap, is
   directional evidence. Not a p-value, but the random-baseline
   alternative ("+20 pp by chance, three times") gets less likely
   each replication.

2. **Two tasks see strong individual lift (+67 pp each).** 03 and 05
   both benefit from caller context. The signal isn't coming from a
   single fluke task.

3. **The harness measures what it claims to.** Same model, same
   prompts (modulo the redaction), same sandbox — only knob changed
   is whether the LLM sees the caller list.

## What's still squishy

- **Variance is large within conditions.** The cg suite scored 3, 1, 3
  across the three runs (3 trials + pilot was 3). nocg scored 2, 1, 1.
  Single-run conclusions remain unreliable; a +20 pp gap may live
  within noise at smaller N.

- **Task 02 / 04 don't benefit.** The "preserve signature" constraint
  is in BOTH prompt variants, but the 7b model ignores it on the harder
  refactors. CG context doesn't fix model capability gaps.

- **Task 04 went the wrong way.** -33 pp for cg vs no-cg on a 1-vs-0
  basis is well within noise but worth checking if it persists at
  larger N. Possible mechanism: long caller list bloats the prompt
  and crowds out the goal text on the heaviest task.

- **No goal-validation yet.** "PASS" still means compile + caller-compat.
  We don't measure whether the patch actually achieves the stated goal.
  See task 05 in baseline-001 for a known case where PASS was earned
  by ignoring the goal.

## Suggested next experiments

1. **Larger model (gemma4:31b-cloud).** Same suite, see if the bigger
   brain (a) closes the cg/no-cg gap by being competent without
   context, or (b) widens it by USING context more effectively.
2. **N=10 on tasks 03 and 05 specifically.** Lock in the +67pp claim
   with a sample large enough to push it past noise.
3. **Goal-achievement scoring axis.** Even just "did the LLM produce
   a fn whose signature is recognisable?" filters out task-05-style
   compiles-but-ignored-the-goal patches.

## Reproducibility

```sh
tools/draug-eval-runner/run-trials.sh 3   # ~30 min
python tools/draug-eval-runner/aggregate.py \
    tools/draug-eval-runner/output-cg-r* \
    tools/draug-eval-runner/output-nocg-r* \
    --csv tools/draug-eval-runner/n3-trial.csv
```

CSV is at `tools/draug-eval-runner/n3-trial.csv` for re-analysis.
