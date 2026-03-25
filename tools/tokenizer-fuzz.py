"""
Differential tokenizer fuzzer: compares Folkering's libtensor tokenizer
against llama-cpp-python across thousands of test cases.

Usage: py -3.12 tools/tokenizer-fuzz.py [--count 10000] [--model boot/model.gguf]
"""
import subprocess, json, random, string, sys, os, time

os.chdir(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))

MODEL = sys.argv[sys.argv.index("--model") + 1] if "--model" in sys.argv else "boot/model.gguf"
COUNT = int(sys.argv[sys.argv.index("--count") + 1]) if "--count" in sys.argv else 5000
TOK_BIN = os.path.join(os.getcwd(), "tools", "tokenizer-test", "target", "release", "tok-test.exe")

# Build if needed
if not os.path.exists(TOK_BIN):
    print("Building tok-test...")
    r = subprocess.run(["cargo", "build", "--manifest-path", "tools/tokenizer-test/Cargo.toml", "--release"],
                       capture_output=True, text=True)
    if r.returncode != 0:
        print(f"Build failed:\n{r.stderr}")
        sys.exit(1)

# Load llama-cpp reference tokenizer
print(f"Loading llama-cpp tokenizer from {MODEL}...")
from llama_cpp import Llama
llm = Llama(model_path=MODEL, n_ctx=32, verbose=False)

def rust_tokenize(text_bytes):
    """Call our Rust tokenizer via subprocess."""
    r = subprocess.run([TOK_BIN, MODEL], input=text_bytes, capture_output=True, timeout=5)
    if r.returncode != 0:
        return None, r.stderr.decode(errors="replace")
    return json.loads(r.stdout), None

def llama_tokenize(text_bytes):
    """Call llama-cpp tokenizer."""
    return llm.tokenize(text_bytes, add_bos=False)

# Test case generators
def gen_chatml_boundaries():
    """ChatML edge cases — the exact bug class we found."""
    cases = [
        b"<|im_start|>system\n",
        b"<|im_end|>\n<|im_start|>user\n",
        b"<|im_start|>assistant\n",
        b"<|im_end|>",
        b"<|im_start|>",
        b"<|im_start|>system\nYou are helpful.\n<|im_end|>\n",
        b"<|im_start|>user\nhi\n<|im_end|>\n<|im_start|>assistant\n",
        b"<|im",  # partial tag
        b"<|im_start",  # partial tag
        b"<|im_start|>system\nYou are Folkering OS, a helpful AI assistant.\n<|im_end|>\n<|im_start|>user\nhi\n<|im_end|>\n<|im_start|>assistant\n",
    ]
    return cases

def gen_single_bytes():
    """Every single byte value (0-255)."""
    return [bytes([b]) for b in range(256)]

def gen_random_ascii(count):
    """Random ASCII strings."""
    cases = []
    for _ in range(count):
        length = random.randint(1, 200)
        text = "".join(random.choices(string.printable, k=length))
        cases.append(text.encode())
    return cases

def gen_random_bytes(count):
    """Random byte sequences including control chars."""
    cases = []
    for _ in range(count):
        length = random.randint(1, 128)
        cases.append(bytes(random.randint(0, 255) for _ in range(length)))
    return cases

def gen_common_phrases():
    """Common English phrases."""
    phrases = [
        b"Hello, world!", b"The quick brown fox", b"12345", b"",
        b"Hello\nWorld", b"Tab\there", b"  spaces  ",
        b"def main():\n    print('hello')\n",
        b"https://example.com/path?q=hello&lang=en",
        b"user@email.com", b"$100.50", b"C:\\Users\\test",
    ]
    return phrases

def gen_unicode():
    """UTF-8 multibyte sequences."""
    phrases = [
        "Héllo wörld".encode(),
        "日本語テスト".encode(),
        "emoji 🎉 test".encode(),
        "café résumé naïve".encode(),
        "Ğ ĉ Ċ".encode(),  # GPT-2 byte encoding chars
    ]
    return phrases

# Run differential tests
print(f"\n{'='*60}")
print(f"Differential Tokenizer Fuzzer")
print(f"{'='*60}")
print(f"Model: {MODEL}")
print(f"Target: {COUNT} test cases")
print()

all_cases = []
all_cases.extend([(c, "chatml") for c in gen_chatml_boundaries()])
all_cases.extend([(c, "single_byte") for c in gen_single_bytes()])
all_cases.extend([(c, "common") for c in gen_common_phrases()])
all_cases.extend([(c, "unicode") for c in gen_unicode()])
remaining = max(0, COUNT - len(all_cases))
all_cases.extend([(c, "random_ascii") for c in gen_random_ascii(remaining // 2)])
all_cases.extend([(c, "random_bytes") for c in gen_random_bytes(remaining - remaining // 2)])

random.shuffle(all_cases)

passes = 0
failures = []
errors = 0
t0 = time.time()

for i, (test_input, category) in enumerate(all_cases):
    if i > 0 and i % 500 == 0:
        elapsed = time.time() - t0
        rate = i / elapsed
        print(f"  [{i}/{len(all_cases)}] {passes} pass, {len(failures)} fail, {errors} err ({rate:.0f} cases/sec)")

    # Get reference tokens
    try:
        ref_tokens = llama_tokenize(test_input)
    except Exception as e:
        errors += 1
        continue

    # Get our tokens
    rust_tokens, err = rust_tokenize(test_input)
    if rust_tokens is None:
        errors += 1
        continue

    # Compare
    if rust_tokens == list(ref_tokens):
        passes += 1
    else:
        failures.append({
            "input": test_input,
            "category": category,
            "rust": rust_tokens,
            "ref": list(ref_tokens),
        })

elapsed = time.time() - t0

# Report
print(f"\n{'='*60}")
print(f"RESULTS ({elapsed:.1f}s)")
print(f"{'='*60}")
print(f"  Total:    {len(all_cases)}")
print(f"  Pass:     {passes}")
print(f"  Fail:     {len(failures)}")
print(f"  Errors:   {errors}")
print(f"  Rate:     {len(all_cases)/elapsed:.0f} cases/sec")

if failures:
    print(f"\n{'='*60}")
    print(f"FAILURES (first 20):")
    print(f"{'='*60}")
    os.makedirs("tools/fuzz-failures", exist_ok=True)
    for i, f in enumerate(failures[:20]):
        inp = f["input"]
        print(f"\n  [{i}] category={f['category']}")
        print(f"      input: {inp!r} ({len(inp)} bytes)")
        print(f"      rust:  {f['rust'][:15]}{'...' if len(f['rust'])>15 else ''} ({len(f['rust'])} tokens)")
        print(f"      ref:   {f['ref'][:15]}{'...' if len(f['ref'])>15 else ''} ({len(f['ref'])} tokens)")

    # Save failures for analysis
    with open("tools/fuzz-failures/latest.json", "w") as fp:
        json.dump([{
            "input_hex": f["input"].hex(),
            "input_repr": repr(f["input"]),
            "category": f["category"],
            "rust_tokens": f["rust"],
            "ref_tokens": f["ref"],
        } for f in failures], fp, indent=2)
    print(f"\n  Full failures saved to tools/fuzz-failures/latest.json")
else:
    print(f"\n  ALL PASS!")

sys.exit(1 if failures else 0)
