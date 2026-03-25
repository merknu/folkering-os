"""
Regression tests for the BPE tokenizer with special token handling.
Validates parity with HuggingFace/llama-cpp tokenization.

Usage: py -3.12 tools/tokenizer-regression.py
"""
import subprocess, json, sys, os

os.chdir(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))

TOK_BIN = os.path.join(os.getcwd(), "tools", "tokenizer-test", "target", "release", "tok-test.exe")
MODEL = "boot/model.gguf"

def tokenize(text_bytes):
    r = subprocess.run([TOK_BIN, MODEL], input=text_bytes, capture_output=True, timeout=5)
    assert r.returncode == 0, f"tok-test failed: {r.stderr.decode()}"
    return json.loads(r.stdout)

passed = 0
failed = 0

def test(name, input_bytes, expected=None, expected_count=None, count_range=None):
    global passed, failed
    tokens = tokenize(input_bytes)
    ok = True
    reasons = []

    if expected is not None and tokens != expected:
        ok = False
        reasons.append(f"expected {expected}, got {tokens}")
    if expected_count is not None and len(tokens) != expected_count:
        ok = False
        reasons.append(f"expected {expected_count} tokens, got {len(tokens)}")
    if count_range is not None:
        lo, hi = count_range
        if not (lo <= len(tokens) <= hi):
            ok = False
            reasons.append(f"expected {lo}-{hi} tokens, got {len(tokens)}")

    status = "PASS" if ok else "FAIL"
    print(f"  [{status}] {name} ({len(tokens)} tokens)")
    if not ok:
        for r in reasons:
            print(f"         {r}")
        failed += 1
    else:
        passed += 1

print("Folkering OS Tokenizer Regression Tests")
print("=" * 50)

# Test 1: <|im_start|> emitted as special token ID 1
test(
    "ChatML <|im_start|>system tag",
    b"<|im_start|>system\n",
    expected=[1, 9690, 198],
)

# Test 2: <|im_end|> emitted as special token ID 2
test(
    "ChatML <|im_end|> tag",
    b"<|im_end|>",
    expected=[2],
)

# Test 3: Full ChatML prompt — must match HuggingFace exactly
full_prompt = b"<|im_start|>system\nYou are a helpful AI assistant.\n<|im_end|>\n<|im_start|>user\nhi\n<|im_end|>\n<|im_start|>assistant\n"
test(
    "Full ChatML prompt (HuggingFace parity)",
    full_prompt,
    expected=[1,9690,198,2683,359,253,5356,5646,11173,30,198,2,198,1,4093,198,6004,198,2,198,1,520,9531,198],
)

# Test 4: Simple ASCII
test(
    "Simple ASCII 'hello world'",
    b"hello world",
    expected_count=2,
)

# Test 5: Empty input
test("Empty input", b"", expected=[])

# Test 6: Single newline
test("Single newline", b"\n", expected=[198])

# Test 7: Single space
test("Single space", b" ", expected=[216])

# Test 8: Hello, how are you? — exact match with HuggingFace
test(
    "Hello how are you (HuggingFace parity)",
    b"Hello, how are you?",
    expected=[19556, 28, 638, 359, 346, 47],
)

# Test 9: Previously failing case (greedy LPM divergence)
test(
    "BPE merge ordering _[\"G",
    b'_["G',
    expected=[79, 2790, 55],
)

print()
print(f"Results: {passed} passed, {failed} failed")
sys.exit(1 if failed else 0)
