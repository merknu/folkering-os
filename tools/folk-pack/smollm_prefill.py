"""
SmolLM2-135M GGUF prefill test.
Feeds a hardcoded token sequence and prints top-10 logits + special token logits.
"""
import sys
import ctypes
import numpy as np
from llama_cpp import Llama

MODEL_PATH = "C:/Users/merkn/folkering/folkering-os/boot/model.gguf"
TOKENS = [1, 1, 4093, 198, 28120, 198, 2, 198, 1, 11173, 198]

print(f"Loading model: {MODEL_PATH}")
llm = Llama(model_path=MODEL_PATH, n_ctx=512, n_batch=512, logits_all=True, verbose=False)

print(f"Token sequence ({len(TOKENS)} tokens): {TOKENS}")

# Decode each token to see what they represent
print("\nToken decoding:")
for i, tok in enumerate(TOKENS):
    try:
        text = llm.detokenize([tok]).decode("utf-8", errors="replace")
    except Exception:
        text = "<error>"
    print(f"  [{i}] token {tok} -> {repr(text)}")

# Eval/prefill: feed all tokens
print("\nRunning prefill...")
llm.reset()
llm.eval(TOKENS)

# Get logits for the last token position
n_vocab = llm.n_vocab()
print(f"Vocabulary size: {n_vocab}")

# Access the logits via the low-level C API
# After eval, get_logits returns pointer to logits for all positions when logits_all=True
import llama_cpp.llama_cpp as llama_raw

# Get logits pointer - points to (n_tokens * n_vocab) floats when logits_all=True
logits_ptr = llama_raw.llama_get_logits(llm._ctx.ctx)
if not logits_ptr:
    print("ERROR: logits pointer is null!")
    sys.exit(1)

# We want the last token's logits
last_pos = len(TOKENS) - 1
# Cast to float array and extract last position
all_logits = np.ctypeslib.as_array(
    ctypes.cast(logits_ptr, ctypes.POINTER(ctypes.c_float)),
    shape=(len(TOKENS), n_vocab)
)
logits = all_logits[last_pos].copy()

# Top-10 by logit value
top10_indices = np.argsort(logits)[::-1][:10]
print(f"\n{'='*60}")
print(f"TOP-10 LOGITS (after prefill of {len(TOKENS)} tokens)")
print(f"{'='*60}")
print(f"{'Rank':<6}{'Token ID':<12}{'Logit':<16}{'Text'}")
print(f"{'-'*60}")
for rank, idx in enumerate(top10_indices):
    try:
        text = llm.detokenize([int(idx)]).decode("utf-8", errors="replace")
    except Exception:
        text = "<error>"
    print(f"{rank+1:<6}{idx:<12}{logits[idx]:<16.6f}{repr(text)}")

# Special token logits (indices 0-4)
print(f"\n{'='*60}")
print(f"SPECIAL TOKEN LOGITS (indices 0-4)")
print(f"{'='*60}")
print(f"{'Index':<10}{'Logit':<16}{'Text'}")
print(f"{'-'*40}")
for idx in range(5):
    try:
        text = llm.detokenize([idx]).decode("utf-8", errors="replace")
    except Exception:
        text = "<error>"
    print(f"{idx:<10}{logits[idx]:<16.6f}{repr(text)}")

# Argmax
argmax_id = int(np.argmax(logits))
try:
    argmax_text = llm.detokenize([argmax_id]).decode("utf-8", errors="replace")
except Exception:
    argmax_text = "<error>"
print(f"\n{'='*60}")
print(f"ARGMAX: token {argmax_id} (logit={logits[argmax_id]:.6f}) -> {repr(argmax_text)}")
print(f"{'='*60}")
