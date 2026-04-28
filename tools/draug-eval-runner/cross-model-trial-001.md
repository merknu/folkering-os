# Cross-model trial 001 — qwen2.5-coder:7b vs gemma4:31b-cloud

**Date:** 2026-04-28
**N=3 per (model, condition).** 12 trials total, ~80 min wall.
**Aggregator:** `tools/draug-eval-runner/aggregate.py` (multi-model
mode auto-engages when ≥2 distinct `model` values appear in the data).

## Headline — the gap inverts

| Model | with-CG | no-CG | diff |
|---|:-:|:-:|:-:|
| `qwen2.5-coder:7b`   | 7/15 (46.7%) | 4/15 (26.7%) | **+20.0 pp** |
| `gemma4:31b-cloud`   | 7/15 (46.7%) | 8/14 (57.1%) | **-10.5 pp** |

**The CodeGraph caller list helps the small model and hurts the big one.**

The simplest reading: the 7b coder needs the blast-radius hint to
remember the "preserve signature" constraint; gemma4 already gets
the constraint from prompt + source alone, and the extra ~500 bytes
of caller paths just dilutes the goal text and confuses it.

(Note: g4 nocg has 14/15 not 15/15 — task 04 trial 1 timed out
mid-LLM-call and was skipped. Discussed below.)

## Per-task — which tasks the model size flipped

### `qwen2.5-coder:7b`

| Task | with-CG | no-CG | Δpp |
|---|:-:|:-:|:-:|
| 01_pop_i32_slot       | 3/3 | 3/3 | 0 |
| 02_maybe_bounds_check | 0/3 | 0/3 | 0 |
| 03_alloc_pages        | **2/3** | **0/3** | **+67** |
| 04_compile_module     | 0/3 | 1/3 | -33 (noise) |
| 05_push_dec           | **2/3** | **0/3** | **+67** |

### `gemma4:31b-cloud`

| Task | with-CG | no-CG | Δpp |
|---|:-:|:-:|:-:|
| 01_pop_i32_slot       | 3/3 | 3/3 | 0   ← same as 7b |
| 02_maybe_bounds_check | 0/3 | 0/3 | 0   ← same as 7b (capability gap) |
| 03_alloc_pages        | 1/3 | 2/3 | **-33** ← inverted from 7b's +67 |
| 04_compile_module     | 0/3 | 0/2 | 0   ← same as 7b (capability gap) |
| 05_push_dec           | 3/3 | 3/3 | 0   ← model-size lift, 7b had 2/3 vs 0/3 |

### What changed when we scaled the model

1. **Task 05 became free.** 7b: cg-dependent (2/3 vs 0/3).
   g4: 3/3 in both conditions. Bigger model just gets it.
2. **Task 03 inverted.** 7b: cg helps (+67 pp). g4: cg hurts
   (-33 pp). The small model needed the caller list to pick
   `append`-strategy; the big model picks correctly without it AND
   seems to pick incorrectly more often when given the list.
3. **Tasks 02 and 04 didn't budge.** Both fail in all 4 cells.
   Capability gap, not context gap. We need a different model class
   (or different task design) to move these.
4. **Task 01 didn't budge.** 3/3 across all 4 cells. Caller-insensitive
   refactor at this difficulty for both model sizes.

## What this implies for Draug deployment

Draug picks LLM models per skill level: 7b for L1, 31b for L2/L3.
This trial says **CodeGraph context should be conditional on model
choice**, not always-on:

- For 7b paths (L1): keep the caller list in the prompt — verified
  +20 pp lift.
- For 31b paths (L2/L3): consider dropping the caller list — it
  appears to *hurt* by ~10 pp on this task set. At minimum, run a
  larger N=10 trial before committing to the always-on shape.

The infrastructure cost isn't wasted: we still need CodeGraph's CSR
to compute *which* callers exist (so post-patch caller-compat
verification works regardless of whether the LLM saw the list). The
prompt-injection layer is the only knob this trial questions.

## Caveats — read before celebrating the inversion

- **N=3 per cell.** 12 cells total (2 models × 2 conditions × 5 tasks → 60 datapoints).
  +20 pp / -10.5 pp with N=3 is suggestive, not proof. The
  task-03 inversion (7b +67 → g4 -33) is the strongest individual
  signal, and even that is 2 vs 1 datapoints out of 3.
- **gemma4 nocg task 04 was skipped once** (LLM call timed out at
  the 180 s proxy cap). The "0/2" denominator instead of "0/3"
  doesn't change the conclusion (g4 fails 04 in both conditions)
  but is the kind of missing-data artefact a stricter analysis
  would flag.
- **PASS still measures compile + caller-compat, not goal achievement.**
  An "ignored the goal but compiles" patch counts as PASS. We'd want
  semantic-goal scoring before deploying any prompt-shape change.
- **Cloud variance is its own beast.** gemma4:31b-cloud routes through
  ollama.com — temperature, scheduling, and possibly model version
  drift between runs. A second N=3 next week could land different.
- **Tasks 02 and 04 are the actually interesting ones.** Both
  failed on both models. Whether that's task design (poorly-specified
  goal, ambiguous constraint) or genuine capability gap is undefined
  from this data alone.

## Suggested next experiments

1. **N=10 just on tasks 03 and 05** for both models. Lock in (or
   refute) the +67 / -33 pp claims with enough power to not be
   moved by single-trial luck.
2. **Frontier model (claude-sonnet-4.6 or gpt-5)** — does the gap
   close further (always-pass everywhere) or do tasks 02 and 04
   finally come unstuck?
3. **Goal-achievement scoring** — distinguish "compiles + ignored
   the goal" from "compiles + did what was asked".
4. **Prompt-position experiment** — move callers ABOVE the goal text
   in the prompt, or duplicate the "preserve signature" instruction.
   Either could shift the small-model lift without hurting the
   large model.

## Reproducibility

```sh
tools/draug-eval-runner/run-trials.sh 3 qwen2.5-coder:7b
tools/draug-eval-runner/run-trials.sh 3 gemma4:31b-cloud g4

python tools/draug-eval-runner/aggregate.py \
    tools/draug-eval-runner/output-cg-r* \
    tools/draug-eval-runner/output-nocg-r* \
    tools/draug-eval-runner/output-cg-g4-r* \
    tools/draug-eval-runner/output-nocg-g4-r* \
    --md tools/draug-eval-runner/cross-model-aggregate.md \
    --csv tools/draug-eval-runner/cross-model-trial.csv
```

Per-task JSON in `output-*/<task-id>/score.json`. Each row tagged
with `model` and `codegraph_in_prompt` so the aggregator's
segmentation is robust to dir-naming choices.
