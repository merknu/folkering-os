"""Ground-truth logit dump for Qwen3-4B-Instruct-2507.

Loads the HF model in fp32 on CPU, runs the same ChatML prompt the
Folkering inference task uses, and prints top-10 logits at every
prefill position. Compare against the [INFERENCE] D.3.7 dbg lines
in the serial log to localise where the Rust forward pass starts
diverging from the Python reference.

Usage:
    python tools/qwen3_4b_ref.py
    python tools/qwen3_4b_ref.py --prompt "Hvem er du?" --positions 0,5,13
    python tools/qwen3_4b_ref.py --decode 16   # also greedy-decode 16 tokens

Requires ~16 GiB free RAM for fp32 weights + activations. If your
host can't spare that, pass --bf16 (8 GiB) at the cost of some
precision drift relative to our Q8 inference.
"""

import argparse
import sys
from pathlib import Path

# Windows defaults stdout/stderr to cp1252 which crashes on the
# multilingual tokens Qwen3 emits (Chinese, emoji, etc). Force UTF-8.
if sys.stdout.encoding and sys.stdout.encoding.lower() != "utf-8":
    sys.stdout.reconfigure(encoding="utf-8", errors="replace")
    sys.stderr.reconfigure(encoding="utf-8", errors="replace")

import torch
from transformers import AutoModelForCausalLM, AutoTokenizer

DEFAULT_MODEL = (
    Path.home()
    / ".cache"
    / "huggingface"
    / "hub"
    / "models--Qwen--Qwen3-4B-Instruct-2507"
    / "snapshots"
    / "cdbee75f17c01a7cc42f958dc650907174af0554"
)

DEFAULT_SYSTEM = "Du er en hjelpsom AI-assistent. Svar kort og presist på norsk bokmål."


def build_prompt(system: str, user: str) -> str:
    return (
        f"<|im_start|>system\n{system}<|im_end|>\n"
        f"<|im_start|>user\n{user}<|im_end|>\n"
        f"<|im_start|>assistant\n"
    )


def parse_positions(spec: str, max_pos: int) -> list[int]:
    if spec == "all":
        return list(range(max_pos))
    out = []
    for part in spec.split(","):
        part = part.strip()
        if not part:
            continue
        if "-" in part:
            a, b = part.split("-")
            out.extend(range(int(a), int(b) + 1))
        else:
            out.append(int(part))
    return [p for p in out if 0 <= p < max_pos]


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--model", default=str(DEFAULT_MODEL))
    ap.add_argument("--system", default=DEFAULT_SYSTEM)
    ap.add_argument("--user", default="Hvem er du?")
    ap.add_argument(
        "--positions",
        default="all",
        help="Comma/range list of prefill positions, or 'all'",
    )
    ap.add_argument("--top", type=int, default=10)
    ap.add_argument(
        "--decode",
        type=int,
        default=0,
        help="If > 0, also greedy-decode N tokens after prefill",
    )
    ap.add_argument("--bf16", action="store_true", help="Load weights in bf16")
    args = ap.parse_args()

    dtype = torch.bfloat16 if args.bf16 else torch.float32
    print(f"[ref] loading {args.model} as {dtype} on cpu ...", file=sys.stderr)

    tok = AutoTokenizer.from_pretrained(args.model)
    model = AutoModelForCausalLM.from_pretrained(
        args.model, torch_dtype=dtype, device_map="cpu"
    )
    model.eval()

    prompt = build_prompt(args.system, args.user)
    enc = tok(prompt, return_tensors="pt")
    ids = enc.input_ids
    n = ids.shape[1]
    print(f"[ref] prompt = {n} tokens, ids = {ids[0].tolist()}", file=sys.stderr)
    print(f"[ref] decoded = {tok.decode(ids[0])!r}", file=sys.stderr)

    with torch.no_grad():
        out = model(ids, return_dict=True)
        logits = out.logits[0].float()  # [seq_len, vocab]

    positions = parse_positions(args.positions, n)
    for pos in positions:
        top = torch.topk(logits[pos], args.top)
        idxs = top.indices.tolist()
        vals = [round(v, 3) for v in top.values.tolist()]
        decs = [repr(tok.decode([i])) for i in idxs]
        pairs = list(zip(idxs, decs, vals))
        print(f"[ref] pos={pos:02d} top{args.top}=", end="")
        print("[" + ", ".join(f"({i}, {d}, {v})" for i, d, v in pairs) + "]")

    if args.decode > 0:
        print(f"[ref] greedy decode {args.decode} tokens ...", file=sys.stderr)
        gen = model.generate(
            ids,
            max_new_tokens=args.decode,
            do_sample=False,
            temperature=1.0,
            repetition_penalty=1.0,
            pad_token_id=tok.eos_token_id,
        )
        new = gen[0, n:].tolist()
        print(f"[ref] generated_ids = {new}")
        print(f"[ref] generated_text = {tok.decode(new)!r}")

    return 0


if __name__ == "__main__":
    sys.exit(main())
