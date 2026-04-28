# Aggregated eval results — multi-model

## Model: `gemma4:31b-cloud`
# Aggregated eval results

Pass-rate is `passes / runs` per (task, condition).
Errors columns are median / max across runs.

| Task | with-CG pass | with-CG err (med/max) | no-CG pass | no-CG err (med/max) | Δpr |
|------|:-:|:-:|:-:|:-:|:-:|
| `01_pop_i32_slot` | 3/3 | 0/0 | 3/3 | 0/0 | +0 pp |
| `02_maybe_bounds_check` | 0/3 | 6/6 | 0/3 | 6/8 | +0 pp |
| `03_alloc_pages` | 1/3 | 2/2 | 2/3 | 0/2 | -33 pp |
| `04_compile_module` | 0/3 | 3/3 | 0/2 | 3/3 | +0 pp |
| `05_push_dec` | 3/3 | 0/0 | 3/3 | 0/0 | +0 pp |

**Suite totals:**
- with-CG: 7 / 15 (46.7%)
- no-CG:   8 / 14 (57.1%)
- diff:    -10.5 pp

_Runs per condition: with-CG=3, no-CG=3_

## Model: `qwen2.5-coder:7b`
# Aggregated eval results

Pass-rate is `passes / runs` per (task, condition).
Errors columns are median / max across runs.

| Task | with-CG pass | with-CG err (med/max) | no-CG pass | no-CG err (med/max) | Δpr |
|------|:-:|:-:|:-:|:-:|:-:|
| `01_pop_i32_slot` | 3/3 | 0/0 | 3/3 | 0/0 | +0 pp |
| `02_maybe_bounds_check` | 0/3 | 10/16 | 0/3 | 6/8 | +0 pp |
| `03_alloc_pages` | 2/3 | 0/3 | 0/3 | 2/5 | +67 pp |
| `04_compile_module` | 0/3 | 3/4 | 1/3 | 3/4 | -33 pp |
| `05_push_dec` | 2/3 | 0/2 | 0/3 | 2/2 | +67 pp |

**Suite totals:**
- with-CG: 7 / 15 (46.7%)
- no-CG:   4 / 15 (26.7%)
- diff:    +20.0 pp

_Runs per condition: with-CG=3, no-CG=3_

## Cross-model headline

| Model | with-CG pass | no-CG pass | diff |
|---|:-:|:-:|:-:|
| `gemma4:31b-cloud` | 7/15 (46.7%) | 8/14 (57.1%) | -10.5 pp |
| `qwen2.5-coder:7b` | 7/15 (46.7%) | 4/15 (26.7%) | +20.0 pp |
