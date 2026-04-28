# CodeGraph caller-list policy

## What this is

`draug-eval` builds an LLM prompt that contains a "Blast radius —
callers from the static call-graph" section by default. The
`--cg-policy` flag controls when that section is included.

| Policy | Behaviour | When to use |
|---|---|---|
| `always` (default) | Include the caller list for every model. Matches the historic prompt shape; what every prior trial measured. | Default for backwards compat. Matches what compositor's autonomous loop currently does (always include). |
| `never` | Drop the caller list entirely. Same as `--no-codegraph`. | Ablation runs. |
| `by-model` | Include for small models, exclude for known-large ones. | Production after we have N≥10 evidence per model. **Use cautiously today** — the data only strongly supports the small-model branch. |

## Evidence behind `by-model`

### Small models: include callers (strong evidence)

`qwen2.5-coder:7b`, replicated 4× (3 single-shot pilots + N=3 trial):

  with-CG:  7/15  (46.7 %)
  no-CG:    4/15  (26.7 %)
  diff:     +20 pp

Every measurement showed the same direction. The 7b coder ignores
the "preserve signature" constraint on the harder tasks unless the
prompt explicitly enumerates the callers it would break.

**`by-model` keeps the caller list for: ≤8b parameters, no detected
size tag (defaults to "small / unknown → include").**

### Large models: exclude callers (weak evidence)

`gemma4:31b-cloud`, N=3 single-batch:

  top-position (original):
    with-CG:  7/15  (46.7 %)
    no-CG:    8/14  (57.1 %)
    diff:     -10.4 pp

That **looked** like CG hurts the bigger model. But the position
experiment then ran another N=3 cloud session of `no-CG` (identical
prompt) and got 6/15 (40 %), a 17 pp swing on the same input. So
the -10.4 pp diff might be entirely cloud-session variance.

Honest reading: **for large models the effect is unproven, possibly
small or zero.** The `by-model` policy excludes anyway because:

1. Removing context can't hurt much when a competent model can
   do the task from source alone (5 of 5 tasks where the bigger
   model was capable, it scored 3/3 in BOTH conditions).
2. The caller list is ~500 bytes of paths that prompt-cache poorly
   across runs and slow down cold-prompt processing.
3. Re-introducing it later (flip back to `always`) is a one-line
   policy change.

The cost of being wrong is small in either direction; we pick the
direction that gets us measurable speed savings on cloud calls.

**`by-model` excludes the caller list for: ≥13b parameter tags,
or `cloud` substring in the model name.** Update the marker list in
`main.rs::CgPolicy::ByModel::includes_callers` as we add models.

## What this policy does NOT solve

- **Compositor / Draug deployment.** The eval runner has the policy,
  but the actual Draug code in `userspace/compositor` still always
  fetches the caller list (when it fetches at all — see notes on
  the autonomous loop in earlier PRs). Wiring this policy into the
  compositor's prompt construction is a separate change that
  requires booting the OS to verify.

- **Goal-achievement scoring.** PASS still means compile +
  caller-compat. A patch that compiles but ignores the goal counts
  as PASS; that's not what we ultimately care about.

- **Cloud variance.** Until the proxy LLM endpoint sets `temperature=0`
  and we run N≥10 per cell in matched cloud sessions, fine-grained
  comparisons on cloud-routed models stay noisy.

## When to revisit

Flip the `by-model` exclusion list (or replace the policy entirely)
when any of these land:

- N=10 same-batch trial on gemma4 (or a successor) confirms or
  refutes the -10 pp result.
- Frontier model trial (claude-sonnet-4.6 / gpt-5) — does the
  capability gap on tasks 02 and 04 close?
- A goal-achievement scoring axis lets us see whether "compiles
  with no caller list" patches actually do what was asked.
