#!/usr/bin/env python3
"""Folkering OS `.tokb` writer (BPE tokenizer binary format).

Spec lives in `userspace/inference/src/tokenizer.rs`:

    magic(4) + version(2) + reserved(2) + n_tokens(4) + n_merges(4)
    [token_offsets : u32 × (n_tokens + 1)]
    [token_bytes   : UTF-8 packed]
    [merges        : (left u32, right u32, result u32) × n_merges]

D.3.1 ships with a synthetic 256-byte tokenizer + a few merge rules
so the inference task can prove encode/decode end-to-end without
needing a real Qwen `tokenizer.json` (151k tokens, ~7 MiB). That
plumbing — convert HuggingFace `tokenizer.json` → `.tokb` — lands in
D.3.1.b.
"""

import struct
from typing import List, Tuple

MAGIC = b"TOK1"
VERSION = 1


def write_tokb(
    vocab: List[str],
    merges: List[Tuple[int, int, int]],
) -> bytes:
    """Pack a vocab + merges list into a `.tokb` blob.

    `merges` must be in priority order — index 0 is highest priority.
    Each merge is `(left_token_id, right_token_id, result_token_id)`.
    """
    n_tokens = len(vocab)
    n_merges = len(merges)

    # Build the byte section + offsets table.
    token_bytes = bytearray()
    offsets = [0]
    for tok in vocab:
        token_bytes += tok.encode("utf-8")
        offsets.append(len(token_bytes))

    out = bytearray()
    # Header (16 bytes).
    out += MAGIC
    out += struct.pack("<HH", VERSION, 0)  # version + reserved
    out += struct.pack("<II", n_tokens, n_merges)
    # Offsets (n_tokens + 1) u32s.
    for o in offsets:
        out += struct.pack("<I", o)
    # Token bytes.
    out += token_bytes
    # Merges (12 bytes each).
    for (l, r, res) in merges:
        out += struct.pack("<III", l, r, res)
    return bytes(out)


def make_synthetic_tokenizer() -> bytes:
    """Build a deterministic test tokenizer.

    Vocab:
      IDs 0..255    : every single byte (token strings are the byte's
                      UTF-8 if printable, else the literal byte chr).
                      Most are 1 char; non-ASCII bytes use Python's
                      bytes_to_unicode trick so the token string is
                      always a valid UTF-8 string our Rust parser can
                      decode.
      ID 256        : "He"  (merge of 'H' + 'e')
      ID 257        : "ll"  (merge of 'l' + 'l')
      ID 258        : "Hi"  (merge of 'H' + 'i')

    Merges (priority order, index 0 = highest):
      ('H', 'e') -> 256
      ('l', 'l') -> 257
      ('H', 'i') -> 258

    Reference:
      encode("Hi")    → [258]
      encode("Hell")  → ['H', 'e', 'l', 'l']
                      → apply (H,e) → [256, 'l', 'l']
                      → apply (l,l) → [256, 257]
                      → result: [256, 257]
      decode([256, 257]) → "Hell"
    """
    # Build 256 base tokens. For printable ASCII (0x20-0x7E) we use the
    # actual char. For non-ASCII / control bytes we use a placeholder
    # encoding that's distinct per byte but valid UTF-8 (we just hex-
    # encode them as `<HH>` so the round-trip is unambiguous in our
    # synthetic test). Real Qwen2.5 uses GPT-2 byte-to-unicode here;
    # D.3.1.b lifts that.
    vocab: List[str] = []
    for b in range(256):
        if 0x20 <= b <= 0x7E:
            vocab.append(chr(b))
        else:
            vocab.append(f"<{b:02x}>")
    # Merge tokens.
    vocab.append("He")  # 256
    vocab.append("ll")  # 257
    vocab.append("Hi")  # 258

    # Merges in priority order.
    merges = [
        (ord('H'), ord('e'), 256),
        (ord('l'), ord('l'), 257),
        (ord('H'), ord('i'), 258),
    ]

    return write_tokb(vocab, merges)
