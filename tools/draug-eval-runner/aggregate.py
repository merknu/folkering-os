#!/usr/bin/env python3
"""Aggregate `score.json` files across multiple `eval --all` runs.

Usage
-----
    aggregate.py [DIRS...]

Each directory should contain `<task-id>/score.json` files produced
by `draug-eval score` (or `eval`). The script segments runs by their
`codegraph_in_prompt` field (set by the runner itself, not by dir
name) so the analysis is robust to naming conventions.

Output
------
1. A Markdown summary table written to stdout. Columns:
     pass-rate per task per condition,
     median + max error count per task per condition,
     overall pass-rate per condition.
2. Optional CSV with per-(task, condition, run) rows if --csv given.

We compute pass-rate as exact (passes / total) — small N, so don't
over-statisticize. The Markdown output is primarily a human-readable
snapshot of the trial data; downstream tooling that wants real
statistical tests can consume the CSV.
"""

from __future__ import annotations

import argparse
import json
import sys
from collections import defaultdict
from pathlib import Path
from statistics import median


def load_run(directory: Path) -> list[dict]:
    """Load every score.json under `directory/<task-id>/`."""
    out = []
    if not directory.is_dir():
        print(f"[aggregate] WARN: {directory} not a directory; skipping",
              file=sys.stderr)
        return out
    for task_dir in sorted(directory.iterdir()):
        score = task_dir / "score.json"
        if not score.exists():
            continue
        try:
            data = json.loads(score.read_text())
        except (OSError, json.JSONDecodeError) as e:
            print(f"[aggregate] WARN: {score}: {e}", file=sys.stderr)
            continue
        data["__source_dir"] = str(directory)
        out.append(data)
    return out


def main() -> int:
    # Force UTF-8 stdout on Windows so the markdown table's `Δ` etc.
    # don't crash on the default cp1252 console codec.
    if sys.stdout.encoding and sys.stdout.encoding.lower() != "utf-8":
        try:
            sys.stdout.reconfigure(encoding="utf-8")
        except (AttributeError, OSError):
            pass

    ap = argparse.ArgumentParser()
    ap.add_argument("dirs", nargs="+", type=Path,
                    help="Output directories to aggregate")
    ap.add_argument("--csv", type=Path,
                    help="Optional path to write per-row CSV")
    ap.add_argument("--md", type=Path,
                    help="Write Markdown summary here instead of stdout")
    args = ap.parse_args()

    rows: list[dict] = []
    for d in args.dirs:
        rows.extend(load_run(d))

    if not rows:
        print("[aggregate] no rows loaded", file=sys.stderr)
        return 1

    # Segment by `codegraph_in_prompt` (true → with-CG, false → no-CG).
    # If the field is missing (older runs), infer from dir name as a
    # fallback so legacy data still classifies correctly.
    def condition_of(row: dict) -> str:
        if "codegraph_in_prompt" in row:
            return "cg" if row["codegraph_in_prompt"] else "nocg"
        # Legacy fallback.
        if "no-cg" in row["__source_dir"] or "nocg" in row["__source_dir"]:
            return "nocg"
        return "cg"

    def model_of(row: dict) -> str:
        return row.get("model", "unknown")

    # Group by (model, condition, task) so we can build per-model tables
    # and overall comparisons. When all rows are from a single model,
    # the report collapses gracefully into the one-model view.
    models = sorted({model_of(r) for r in rows})

    by_cond_task: dict[tuple[str, str], list[dict]] = defaultdict(list)
    for r in rows:
        by_cond_task[(condition_of(r), r["task_id"])].append(r)

    task_ids = sorted({r["task_id"] for r in rows})
    conds = ["cg", "nocg"]

    md_parts = []
    if len(models) > 1:
        # Per-model tables first, then a cross-model summary.
        for m in models:
            sub = [r for r in rows if model_of(r) == m]
            sub_by_cond = defaultdict(list)
            for r in sub:
                sub_by_cond[(condition_of(r), r["task_id"])].append(r)
            md_parts.append(f"## Model: `{m}`\n")
            md_parts.append(build_markdown(sub_by_cond, task_ids, conds))
            md_parts.append("\n\n")
        md_parts.append("## Cross-model headline\n\n")
        md_parts.append(cross_model_table(rows, models, condition_of, model_of))
        md = "# Aggregated eval results — multi-model\n\n" + "".join(md_parts)
    else:
        md = build_markdown(by_cond_task, task_ids, conds)
    if args.md:
        args.md.write_text(md, encoding="utf-8")
        print(f"[aggregate] wrote markdown -> {args.md}", file=sys.stderr)
    else:
        print(md)

    if args.csv:
        write_csv(args.csv, rows, condition_of)
        print(f"[aggregate] wrote csv → {args.csv}", file=sys.stderr)

    return 0


def build_markdown(by_cond_task, task_ids, conds) -> str:
    lines = []
    lines.append("# Aggregated eval results")
    lines.append("")
    lines.append("Pass-rate is `passes / runs` per (task, condition).")
    lines.append("Errors columns are median / max across runs.")
    lines.append("")
    lines.append("| Task | with-CG pass | with-CG err (med/max) | no-CG pass | no-CG err (med/max) | Δpr |")
    lines.append("|------|:-:|:-:|:-:|:-:|:-:|")

    cg_total_pass = cg_total_runs = nocg_total_pass = nocg_total_runs = 0
    for tid in task_ids:
        cg_runs = by_cond_task.get(("cg", tid), [])
        nocg_runs = by_cond_task.get(("nocg", tid), [])
        cg_pr, cg_em, cg_eM = stats(cg_runs)
        ng_pr, ng_em, ng_eM = stats(nocg_runs)
        delta = pretty_delta(cg_pr, ng_pr)
        cg_label = pr_label(cg_runs)
        ng_label = pr_label(nocg_runs)
        lines.append(
            f"| `{tid}` | {cg_label} | {cg_em}/{cg_eM} | "
            f"{ng_label} | {ng_em}/{ng_eM} | {delta} |"
        )
        cg_total_pass += sum(1 for r in cg_runs if r["verdict"] == "PASS")
        cg_total_runs += len(cg_runs)
        nocg_total_pass += sum(1 for r in nocg_runs if r["verdict"] == "PASS")
        nocg_total_runs += len(nocg_runs)

    lines.append("")
    lines.append(f"**Suite totals:**")
    lines.append(f"- with-CG: {cg_total_pass} / {cg_total_runs} "
                 f"({fmt_pct(cg_total_pass, cg_total_runs)})")
    lines.append(f"- no-CG:   {nocg_total_pass} / {nocg_total_runs} "
                 f"({fmt_pct(nocg_total_pass, nocg_total_runs)})")
    if cg_total_runs and nocg_total_runs:
        cg_pct = cg_total_pass / cg_total_runs
        ng_pct = nocg_total_pass / nocg_total_runs
        lines.append(f"- diff:    {(cg_pct - ng_pct) * 100:+.1f} pp")
    lines.append("")

    runs_per_cond = {
        c: len({r["__source_dir"] for r in [x for (cd, _), xs in by_cond_task.items()
                                             if cd == c for x in xs]})
        for c in conds
    }
    lines.append(f"_Runs per condition: with-CG={runs_per_cond.get('cg', 0)}, "
                 f"no-CG={runs_per_cond.get('nocg', 0)}_")
    return "\n".join(lines)


def cross_model_table(rows, models, condition_of, model_of) -> str:
    lines = []
    lines.append("| Model | with-CG pass | no-CG pass | diff |")
    lines.append("|---|:-:|:-:|:-:|")
    for m in models:
        cg_rows = [r for r in rows if model_of(r) == m and condition_of(r) == "cg"]
        ng_rows = [r for r in rows if model_of(r) == m and condition_of(r) == "nocg"]
        cg_p = sum(1 for r in cg_rows if r["verdict"] == "PASS")
        ng_p = sum(1 for r in ng_rows if r["verdict"] == "PASS")
        cg_n = len(cg_rows)
        ng_n = len(ng_rows)
        cg_lbl = f"{cg_p}/{cg_n} ({fmt_pct(cg_p, cg_n)})" if cg_n else "—"
        ng_lbl = f"{ng_p}/{ng_n} ({fmt_pct(ng_p, ng_n)})" if ng_n else "—"
        if cg_n and ng_n:
            diff = (cg_p / cg_n - ng_p / ng_n) * 100
            diff_lbl = f"{diff:+.1f} pp"
        else:
            diff_lbl = "—"
        lines.append(f"| `{m}` | {cg_lbl} | {ng_lbl} | {diff_lbl} |")
    return "\n".join(lines) + "\n"


def stats(runs):
    if not runs:
        return ("—", "—", "—")
    passes = sum(1 for r in runs if r["verdict"] == "PASS")
    pr = f"{passes}/{len(runs)}"
    errs = [r["cargo_check"]["error_count"] for r in runs]
    return (pr, str(int(median(errs))), str(max(errs)))


def pr_label(runs):
    if not runs:
        return "—"
    passes = sum(1 for r in runs if r["verdict"] == "PASS")
    return f"{passes}/{len(runs)}"


def pretty_delta(cg_pr, ng_pr):
    if cg_pr == "—" or ng_pr == "—":
        return "—"
    cg_p, cg_n = (int(x) for x in cg_pr.split("/"))
    ng_p, ng_n = (int(x) for x in ng_pr.split("/"))
    if cg_n == 0 or ng_n == 0:
        return "—"
    diff = (cg_p / cg_n) - (ng_p / ng_n)
    return f"{diff * 100:+.0f} pp"


def fmt_pct(num, den):
    if den == 0:
        return "—"
    return f"{(num / den) * 100:.1f}%"


def write_csv(path: Path, rows, condition_of):
    import csv
    with path.open("w", newline="") as f:
        w = csv.writer(f)
        w.writerow([
            "model", "condition", "task_id", "verdict",
            "error_count", "warning_count",
            "patch_strategy", "patch_chars",
            "elapsed_secs", "source_dir",
        ])
        for r in rows:
            cc = r["cargo_check"]
            w.writerow([
                r.get("model", "unknown"),
                condition_of(r), r["task_id"], r["verdict"],
                cc["error_count"], cc["warning_count"],
                r.get("patch_strategy", ""), r.get("patch_chars", 0),
                cc.get("elapsed_secs", 0), r["__source_dir"],
            ])


if __name__ == "__main__":
    sys.exit(main())
