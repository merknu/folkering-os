"""
Quick regression test for the Double Fault tokenizer bug.
Runs in < 2 seconds. Use as a pre-flight check before builds.

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

def test(name, input_bytes, expected=None, forbidden=None, expected_count=None, count_range=None):
    global passed, failed
    tokens = tokenize(input_bytes)
    ok = True
    reasons = []

    if expected is not None and tokens != expected:
        ok = False
        reasons.append(f"expected {expected}, got {tokens}")
    if forbidden is not None and tokens == forbidden:
        ok = False
        reasons.append(f"matched FORBIDDEN sequence {forbidden}")
    if expected_count is not None and len(tokens) != expected_count:
        ok = False
        reasons.append(f"expected {expected_count} tokens, got {len(tokens)}")
    if count_range is not None:
        lo, hi = count_range
        if not (lo <= len(tokens) <= hi):
            ok = False
            reasons.append(f"expected {lo}-{hi} tokens, got {len(tokens)}")

    # Check no control tokens (0-3) leaked
    for i, t in enumerate(tokens):
        if t <= 3:
            ok = False
            reasons.append(f"control token {t} at position {i}")

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

# Test 1: ChatML <|im_start|> must split into 7 subwords
test(
    "ChatML <|im_start|>system tag",
    b"<|im_start|>system\n",
    expected=[44, 108, 306, 79, 3738, 108, 46, 9690, 198],
    forbidden=[1, 9690, 198],  # The old bug: collapsed to special token 1
)

# Test 2: ChatML <|im_end|> must split into 7 subwords
test(
    "ChatML <|im_end|> tag",
    b"<|im_end|>",
    expected=[44, 108, 306, 79, 486, 108, 46],
    forbidden=[2],  # The old bug: collapsed to special token 2
)

# Test 3: Full ChatML prompt token count
full_prompt = b"<|im_start|>system\nYou are Folkering OS, a helpful AI assistant.\n<|im_end|>\n<|im_start|>user\nhi\n<|im_end|>\n<|im_start|>assistant\n"
test(
    "Full ChatML prompt count",
    full_prompt,
    count_range=(57, 60),  # llama-cpp gives 58, we may give 58-59
)

# Test 4: Simple ASCII
test(
    "Simple ASCII 'hello world'",
    b"hello world",
    expected_count=2,  # "hello" + " world" or "hello" + "Ġworld"
)

# Test 5: Empty input
test(
    "Empty input",
    b"",
    expected=[],
)

# Test 6: Single newline
test("Single newline", b"\n", expected=[198])

# Test 7: Single space
test("Single space", b" ", expected=[216])

print()
print(f"Results: {passed} passed, {failed} failed")
sys.exit(1 if failed else 0)
