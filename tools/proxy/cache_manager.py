"""WASM Cache Manager — Lineage tracking, version snapshots, dream budget.

Manages the wasm_cache/ directory containing:
- {key}.rs  — Rust source code
- {key}.wasm — Compiled WASM binary
- {key}.meta.json — Lineage metadata (version, id, parent, intent)
- versions/{key}/v{n}.* — Version snapshots for rollback
- dream_budget.json — Daily dream API call budget
"""

import os
import json
import hashlib
import datetime

# Cache directory (relative to project root)
_WASM_CACHE_DIR = os.path.join(os.path.dirname(os.path.dirname(os.path.abspath(__file__))), "wasm_cache")
_DREAM_BUDGET_PATH = os.path.join(_WASM_CACHE_DIR, "dream_budget.json")

# Ensure cache directory exists
os.makedirs(_WASM_CACHE_DIR, exist_ok=True)
os.makedirs(os.path.join(_WASM_CACHE_DIR, "versions"), exist_ok=True)

# Dream budget from .env
DREAM_MAX_PER_DAY = 25  # Default, overridden by caller


def set_dream_max(max_calls):
    global DREAM_MAX_PER_DAY
    DREAM_MAX_PER_DAY = max_calls


# ── Cache Key ────────────────────────────────────────────────────────────

def cache_key(prompt):
    """Generate a filesystem-safe cache key from a prompt."""
    safe = prompt.replace(" ", "_").replace("/", "_").replace("\\", "_")
    safe = "".join(c for c in safe if c.isalnum() or c in "_-.")
    if len(safe) > 48:
        h = hashlib.md5(prompt.encode()).hexdigest()[:16]
        safe = safe[:48] + "_" + h
    return safe


# ── Cache Operations ─────────────────────────────────────────────────────

def cache_check(prompt):
    """Check if WASM is cached for a prompt. Returns (wasm_bytes, source_code) or (None, None)."""
    key = cache_key(prompt)
    wasm_path = os.path.join(_WASM_CACHE_DIR, f"{key}.wasm")
    src_path = os.path.join(_WASM_CACHE_DIR, f"{key}.rs")
    if os.path.exists(wasm_path):
        with open(wasm_path, "rb") as f:
            wasm = f.read()
        src = None
        if os.path.exists(src_path):
            with open(src_path, "r") as f:
                src = f.read()
        print(f"[CACHE] Hit: {key} ({len(wasm)} bytes)")
        return wasm, src
    return None, None


def cache_store(prompt, source, wasm_bytes, parent_prompt="", dream_mode=""):
    """Store compiled WASM + source code + metadata with lineage tracking."""
    key = cache_key(prompt)
    with open(os.path.join(_WASM_CACHE_DIR, f"{key}.rs"), "w") as f:
        f.write(source)
    with open(os.path.join(_WASM_CACHE_DIR, f"{key}.wasm"), "wb") as f:
        f.write(wasm_bytes)

    source_id = hashlib.sha256(source.encode()).hexdigest()[:12]

    meta = cache_get_meta(prompt)
    meta["version"] = meta.get("version", 0) + 1
    meta["id"] = source_id

    # Clean description
    desc = prompt
    for prefix in ["gemini generate ", "gemini gen ", "generate ", "agent generate "]:
        if desc.lower().startswith(prefix):
            desc = desc[len(prefix):]
            break
    meta["description"] = desc

    # Lineage tracking
    if parent_prompt:
        parent_meta = cache_get_meta(parent_prompt)
        meta["parent_id"] = parent_meta.get("id", "")
        meta["root_id"] = parent_meta.get("root_id", "") or parent_meta.get("id", "")
        if not meta.get("branch") or meta["branch"] == "main":
            meta["branch"] = parent_meta.get("branch", "main")
        parent_lineage = parent_meta.get("lineage", [])
        meta["lineage"] = parent_lineage + [parent_meta.get("id", "")]
    else:
        meta["root_id"] = source_id
        meta["parent_id"] = ""
        meta["lineage"] = []

    # Dream history
    if dream_mode:
        history = meta.get("dream_history", [])
        history.append({"mode": dream_mode, "time": datetime.datetime.now().isoformat(), "version": meta["version"]})
        meta["dream_history"] = history[-20:]

    if not meta.get("created"):
        meta["created"] = datetime.datetime.now().isoformat()
    meta["last_updated"] = datetime.datetime.now().isoformat()

    # Auto-generate intent metadata
    intent = {
        "purpose": desc,
        "type": "interactive_app" if any(kw in source.lower() for kw in
            ["folk_poll_event", "mouse", "click", "key_down"]) else "visual_widget",
        "mime": "application/wasm",
    }
    caps = []
    if "folk_poll_event" in source: caps.append("input")
    if "folk_draw_rect" in source or "folk_fill_screen" in source: caps.append("graphics")
    if "folk_draw_text" in source: caps.append("text")
    if "folk_get_time" in source or "folk_get_datetime" in source: caps.append("time")
    if "folk_random" in source: caps.append("random")
    if caps: intent["capabilities"] = caps
    meta["intent"] = intent

    cache_set_meta(prompt, meta)
    cache_save_version(prompt, source, wasm_bytes)

    lineage_depth = len(meta.get("lineage", []))
    print(f"[CACHE] Stored: {key} v{meta['version']} id={source_id} depth={lineage_depth} ({len(source)} chars, {len(wasm_bytes)} bytes)")


def cache_save_version(prompt, source, wasm_bytes):
    """Save a version snapshot for rollback."""
    key = cache_key(prompt)
    meta = cache_get_meta(prompt)
    ver = meta.get("version", 1)
    ver_dir = os.path.join(_WASM_CACHE_DIR, "versions", key)
    os.makedirs(ver_dir, exist_ok=True)
    with open(os.path.join(ver_dir, f"v{ver}.rs"), "w") as f:
        f.write(source)
    with open(os.path.join(ver_dir, f"v{ver}.wasm"), "wb") as f:
        f.write(wasm_bytes)
    with open(os.path.join(ver_dir, f"v{ver}.meta.json"), "w") as f:
        f.write(json.dumps(meta))


def cache_rollback(prompt, version):
    """Rollback to a specific version. Returns (wasm_bytes, source, error)."""
    key = cache_key(prompt)
    ver_dir = os.path.join(_WASM_CACHE_DIR, "versions", key)
    wasm_path = os.path.join(ver_dir, f"v{version}.wasm")
    src_path = os.path.join(ver_dir, f"v{version}.rs")
    if not os.path.exists(wasm_path):
        return None, None, f"Version {version} not found for '{key}'"
    with open(wasm_path, "rb") as f:
        wasm = f.read()
    src = None
    if os.path.exists(src_path):
        with open(src_path, "r") as f:
            src = f.read()
    # Restore as current
    with open(os.path.join(_WASM_CACHE_DIR, f"{key}.wasm"), "wb") as f:
        f.write(wasm)
    if src:
        with open(os.path.join(_WASM_CACHE_DIR, f"{key}.rs"), "w") as f:
            f.write(src)
    print(f"[CACHE] Rolled back '{key}' to v{version}")
    return wasm, src, None


# ── Metadata ─────────────────────────────────────────────────────────────

def cache_meta_path(prompt):
    return os.path.join(_WASM_CACHE_DIR, f"{cache_key(prompt)}.meta.json")


def cache_get_meta(prompt):
    path = cache_meta_path(prompt)
    if os.path.exists(path):
        try:
            with open(path, "r") as f:
                return json.loads(f.read())
        except Exception:
            pass
    return {}


def cache_set_meta(prompt, meta):
    with open(cache_meta_path(prompt), "w") as f:
        f.write(json.dumps(meta, indent=2))


# ── App Listing ──────────────────────────────────────────────────────────

def list_all_apps():
    """List all cached apps with their lineage metadata."""
    apps = []
    for f in os.listdir(_WASM_CACHE_DIR):
        if f.endswith(".meta.json"):
            try:
                with open(os.path.join(_WASM_CACHE_DIR, f), "r") as fh:
                    meta = json.loads(fh.read())
                key = f.replace(".meta.json", "")
                apps.append({
                    "key": key,
                    "id": meta.get("id", "?"),
                    "parent_id": meta.get("parent_id", ""),
                    "root_id": meta.get("root_id", ""),
                    "branch": meta.get("branch", "main"),
                    "version": meta.get("version", 1),
                    "description": meta.get("description", key),
                    "dreams": len(meta.get("dream_history", [])),
                    "perfected": meta.get("perfected", False),
                })
            except Exception:
                pass
    return apps


# ── Dream Budget ─────────────────────────────────────────────────────────

def dream_budget_check():
    """Check if dream API calls are within daily budget."""
    today = datetime.date.today().isoformat()
    budget = {"date": today, "calls": 0}
    if os.path.exists(_DREAM_BUDGET_PATH):
        try:
            with open(_DREAM_BUDGET_PATH, "r") as f:
                budget = json.loads(f.read())
        except Exception:
            pass
    if budget.get("date") != today:
        budget = {"date": today, "calls": 0}
    if budget["calls"] >= DREAM_MAX_PER_DAY:
        print(f"[DREAM-BUDGET] Blocked: {budget['calls']}/{DREAM_MAX_PER_DAY} calls used today")
        return False
    return True


def dream_budget_record():
    """Record a dream API call."""
    today = datetime.date.today().isoformat()
    budget = {"date": today, "calls": 0}
    if os.path.exists(_DREAM_BUDGET_PATH):
        try:
            with open(_DREAM_BUDGET_PATH, "r") as f:
                budget = json.loads(f.read())
        except Exception:
            pass
    if budget.get("date") != today:
        budget = {"date": today, "calls": 0}
    budget["calls"] += 1
    with open(_DREAM_BUDGET_PATH, "w") as f:
        f.write(json.dumps(budget))
    print(f"[DREAM-BUDGET] Used: {budget['calls']}/{DREAM_MAX_PER_DAY} today")
