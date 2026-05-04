#!/usr/bin/env python3
"""HuggingFace `tokenizer.json` → Folkering `.tokb` (D.3.1.b).

Lifts a real BPE tokenizer (Qwen2.5 / Qwen3 / Llama-3 family — all
share the GPT-2 byte-level convention) into the no_std-parseable
binary the inference task already understands. The file format is
the v2 extension of `tools/fbin-gen/tokb.py`'s v1: same header +
vocab + merge triples, plus a trailing list of special-token IDs
that the runtime treats as atomic units (so `<|im_start|>` encodes
to a single ID instead of byte-level BPE on its 12 characters).

Usage:

    python tools/fbin-gen/tok_to_tokb.py \\
        --tokenizer ~/.cache/huggingface/hub/models--Qwen--Qwen3-0.6B/.../tokenizer.json \\
        --out boot/iso_root/qwen.tokb

Out of scope today (queued for follow-ups):
- The Tiktoken pre-tokenizer regex (\\p{L}, \\p{N}, etc). The runtime
  currently runs byte-level BPE on whitespace-split chunks, which
  matches HF's output on most inputs but can diverge on punctuation
  + digits boundaries. Verify with `--verify` against the reference
  encoder before trusting the output.
- NFC normalization. ASCII / Latin-1 inputs are unaffected; full
  Unicode tables come with the regex work.
"""

import argparse
import json
import struct
import sys
from typing import Dict, List, Tuple


MAGIC = b"TOK1"
VERSION = 2  # adds special_token_ids list at the end


def gpt2_byte_to_unicode() -> Dict[int, str]:
    """Reproduces GPT-2's byte-to-unicode mapping. Used at encode
    time on the Rust side (256-entry lookup), needed here only to
    sanity-check that the vocab strings really are GPT-2-encoded
    (every base byte's mapped char must appear as a 1-char vocab
    token)."""
    bs = (
        list(range(ord("!"), ord("~") + 1))
        + list(range(ord("¡"), ord("¬") + 1))
        + list(range(ord("®"), ord("ÿ") + 1))
    )
    cs = bs[:]
    n = 0
    for b in range(2**8):
        if b not in bs:
            bs.append(b)
            cs.append(2**8 + n)
            n += 1
    return dict(zip(bs, [chr(c) for c in cs]))


def write_tokb(
    vocab_strings: List[str],
    merges: List[Tuple[int, int, int]],
    special_ids: List[int],
) -> bytes:
    """Pack a vocab + merges + special-id list into `.tokb` v2.

    Layout:
      magic(4) + version(2) + reserved(2) + n_tokens(4) + n_merges(4) + n_special(4) + reserved(4)
      [token_offsets : u32 × (n_tokens + 1)]
      [token_bytes   : UTF-8 packed]
      [merges        : (left u32, right u32, result u32) × n_merges]
      [special_ids   : u32 × n_special]
    """
    n_tokens = len(vocab_strings)
    n_merges = len(merges)
    n_special = len(special_ids)

    token_bytes = bytearray()
    offsets = [0]
    for s in vocab_strings:
        token_bytes += s.encode("utf-8")
        offsets.append(len(token_bytes))

    out = bytearray()
    out += MAGIC
    out += struct.pack("<HH", VERSION, 0)
    out += struct.pack("<III", n_tokens, n_merges, n_special)
    out += struct.pack("<I", 0)  # reserved

    for o in offsets:
        out += struct.pack("<I", o)
    out += token_bytes
    for (l, r, res) in merges:
        out += struct.pack("<III", l, r, res)
    for sid in special_ids:
        out += struct.pack("<I", sid)
    return bytes(out)


def build_vocab_table(
    raw_vocab: Dict[str, int],
    added_tokens: List[dict],
) -> Tuple[List[str], int]:
    """Materialise a `[id] -> token_str` table covering both the
    BPE vocab and the added (special) tokens. Returns the table and
    its length. Caller iterates by id `0..n` to write the offsets
    table.

    Qwen's added_tokens often include IDs past the BPE vocab
    range; we extend the table with placeholders for any gaps so
    the runtime's `O(1)` id-to-string lookup stays valid.
    """
    by_id: Dict[int, str] = {}
    for tok, tid in raw_vocab.items():
        by_id[tid] = tok
    for at in added_tokens:
        # added_tokens can override or extend the vocab. Special
        # tokens like `<|im_start|>` always live here.
        by_id[at["id"]] = at["content"]

    if not by_id:
        return [], 0
    n = max(by_id.keys()) + 1
    table = [""] * n
    for tid, s in by_id.items():
        table[tid] = s
    return table, n


def resolve_merges(
    merges_pairs: List,
    vocab_to_id: Dict[str, int],
) -> List[Tuple[int, int, int]]:
    """Convert HF's `[left_str, right_str]` merges into our
    `(left_id, right_id, result_id)` triples. Skips merges where
    any of the three sub-strings is missing from the vocab — this
    happens occasionally in published tokenizers and the BPE
    algorithm degrades gracefully (the merge just never fires)."""
    triples: List[Tuple[int, int, int]] = []
    skipped = 0
    for entry in merges_pairs:
        # Newer tokenizer.json files emit merges as [left, right];
        # older ones used "left right" (space-separated). Handle
        # both for forward compatibility.
        if isinstance(entry, list):
            if len(entry) != 2:
                skipped += 1
                continue
            left, right = entry
        else:
            parts = entry.split(" ", 1)
            if len(parts) != 2:
                skipped += 1
                continue
            left, right = parts
        result = left + right
        l_id = vocab_to_id.get(left)
        r_id = vocab_to_id.get(right)
        res_id = vocab_to_id.get(result)
        if l_id is None or r_id is None or res_id is None:
            skipped += 1
            continue
        triples.append((l_id, r_id, res_id))
    if skipped:
        print(
            f"[tok_to_tokb] note: {skipped} merge(s) skipped "
            f"(missing vocab entry) — common, expected",
            file=sys.stderr,
        )
    return triples


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--tokenizer", required=True,
                    help="Path to HuggingFace tokenizer.json")
    ap.add_argument("--out", required=True, help="Output .tokb path")
    ap.add_argument("--verify", action="store_true",
                    help="Round-trip a few sample strings using HuggingFace's "
                         "tokenizers crate (Python bindings) and print our "
                         "expected encoder output. Optional but recommended "
                         "before shipping the runtime side.")
    args = ap.parse_args()

    with open(args.tokenizer, encoding="utf-8") as f:
        tk = json.load(f)

    if tk["model"].get("type") != "BPE":
        raise SystemExit(
            f"only BPE tokenizers supported; this is "
            f"{tk['model'].get('type')!r}"
        )

    raw_vocab: Dict[str, int] = tk["model"]["vocab"]
    merges_pairs = tk["model"]["merges"]
    added_tokens: List[dict] = tk.get("added_tokens", [])

    vocab_table, n_tokens = build_vocab_table(raw_vocab, added_tokens)
    # Build the vocab-string-to-id index off the materialised table,
    # so resolve_merges sees the union of base + added vocab.
    vocab_to_id: Dict[str, int] = {}
    for tid, s in enumerate(vocab_table):
        if s:
            # Last write wins on collision; matches HF semantics
            # where added_tokens shadow base vocab on the same id.
            vocab_to_id[s] = tid

    # Sanity-check the GPT-2 byte mapping appears in the vocab.
    bm = gpt2_byte_to_unicode()
    sample_chars = [bm[0x20], bm[0x09], bm[0x0A], bm[ord("!")]]  # space, tab, lf, !
    for ch in sample_chars:
        if ch not in vocab_to_id:
            print(
                f"[tok_to_tokb] warning: GPT-2 byte char {ch!r} "
                f"missing from vocab — runtime byte-fallback path "
                f"may misroute. (Common for non-byte-fallback "
                f"tokenizers; usually harmless.)",
                file=sys.stderr,
            )

    triples = resolve_merges(merges_pairs, vocab_to_id)

    special_ids: List[int] = sorted(
        at["id"] for at in added_tokens if at.get("special")
    )

    print(
        f"[tok_to_tokb] {args.tokenizer}: "
        f"vocab={n_tokens} merges_in={len(merges_pairs)} "
        f"merges_out={len(triples)} special={len(special_ids)}"
    )

    blob = write_tokb(vocab_table, triples, special_ids)
    with open(args.out, "wb") as f:
        f.write(blob)
    print(f"[tok_to_tokb] wrote {args.out} ({len(blob):,} bytes)")

    if args.verify:
        verify(args.tokenizer, vocab_table)


def verify(tokenizer_json_path: str, vocab_table: List[str]):
    """Round-trip a handful of strings through HuggingFace's
    `tokenizers` crate so the reader can compare the IDs the
    Folkering runtime should produce against the canonical
    reference. Skips silently if `tokenizers` isn't installed."""
    samples = [
        "Hello world",
        "Hvem er du?",
        "<|im_start|>user\nHvem er du?<|im_end|>\n<|im_start|>assistant\n",
    ]
    try:
        from tokenizers import Tokenizer
    except ImportError:
        print("[tok_to_tokb] verify: install `tokenizers` for HF reference",
              file=sys.stderr)
        return
    ref = Tokenizer.from_file(tokenizer_json_path)
    print()
    for s in samples:
        ref_ids = ref.encode(s).ids
        ref_pieces = [vocab_table[i] if i < len(vocab_table) else f"<oob:{i}>"
                      for i in ref_ids[:30]]
        print(f"[tok_to_tokb] verify: {s!r}")
        print(f"  HF ids[:30]    = {ref_ids[:30]}")
        print(f"  HF pieces[:30] = {ref_pieces}")
        print(f"  HF len         = {len(ref_ids)}")


if __name__ == "__main__":
    main()
