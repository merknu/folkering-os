import re, time, sys, os, json

with open(sys.argv[1]) as f:
    cfg = json.load(f)

PATTERN = re.compile(cfg["pattern"])
LOG = cfg["log_path"]
TIMEOUT = cfg["timeout"]
CTX = cfg["context_lines"]
FROM_START = cfg["from_start"]

start = time.time()
context = []

while not os.path.exists(LOG):
    if time.time() - start > TIMEOUT:
        print(f"TIMEOUT: {LOG} never appeared after {TIMEOUT}s")
        sys.exit(1)
    time.sleep(0.5)

with open(LOG, "r", errors="replace") as f:
    if not FROM_START:
        f.seek(0, 2)
    while True:
        if time.time() - start > TIMEOUT:
            elapsed = time.time() - start
            print(f"TIMEOUT after {elapsed:.1f}s — pattern not found")
            if context:
                print(f"--- Last {len(context)} lines ---")
                for c in context:
                    print(c)
            sys.exit(1)
        line = f.readline()
        if not line:
            time.sleep(0.1)
            continue
        line = line.rstrip("\n")
        context.append(line)
        if len(context) > CTX:
            context.pop(0)
        if PATTERN.search(line):
            elapsed = time.time() - start
            print(f"MATCH at {elapsed:.1f}s")
            print(f"Matched: {line}")
            print(f"--- Context ({len(context)} lines) ---")
            for c in context:
                print(c)
            sys.exit(0)
