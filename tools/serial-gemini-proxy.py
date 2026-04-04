#!/usr/bin/env python3
"""Serial LLM Proxy — communicates with Folkering OS via COM2 (TCP socket).

Supports multiple LLM providers: Gemini, OpenAI/ChatGPT, and Claude.
Configure via .env file or environment variables.

QEMU exposes COM2 as TCP server on port 4567.
This proxy connects, reads @@GEMINI_REQ@@{json}@@END@@ from COM2,
calls the configured LLM API, and writes @@GEMINI_RESP@@{text}@@END@@ back.

Usage: python serial-gemini-proxy.py
"""

import socket
import json
import urllib.request
import ssl
import time
import sys
import threading
import subprocess
import tempfile
import os
import base64
import shutil

# Add tools/ directory to path for mcp_bridge import
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from context_manager import ContextManager
from mcp_bridge import _retx_queue  # Retransmission queue (global, used in handle_serial timeout check)
_context_mgr = ContextManager(max_tokens=4096)  # Match LLM context window

# ── Configuration (from .env or environment variables) ────────────────────

def load_env():
    """Load .env file from project root into a dict."""
    env = {}
    _env_path = os.path.join(os.path.dirname(os.path.dirname(__file__)), ".env")
    if os.path.exists(_env_path):
        for line in open(_env_path):
            line = line.strip()
            if line and not line.startswith("#") and "=" in line:
                key, val = line.split("=", 1)
                env[key.strip()] = val.strip().strip('"').strip("'")
    return env

_env = load_env()

def cfg(key, default=""):
    """Get config from env var or .env file."""
    return os.environ.get(key, _env.get(key, default))

# ── Hybrid Model Router ──────────────────────────────────────────────────
# Three tiers: FAST (local, cheap), MEDIUM (cloud lite), HEAVY (cloud smart)
# Each task is routed to the cheapest tier that can handle it.

# FAST tier: local Ollama — free, instant, but limited reasoning
FAST_PROVIDER = cfg("FAST_LLM_PROVIDER", "local")
FAST_MODEL = cfg("FAST_LLM_MODEL", "qwen2.5-coder:7b")
FAST_URL = cfg("FAST_LLM_URL", "http://localhost:11434/v1")

# MEDIUM tier: cloud lite — cheap, good for code gen + simple tool calls
MEDIUM_PROVIDER = cfg("MEDIUM_LLM_PROVIDER", "gemini")
MEDIUM_MODEL = cfg("MEDIUM_LLM_MODEL", "gemini-3.1-flash-lite-preview")
MEDIUM_URL = cfg("MEDIUM_LLM_URL", "https://generativelanguage.googleapis.com/v1beta")

# HEAVY tier: cloud smart — expensive, for complex multi-step reasoning
HEAVY_PROVIDER = cfg("HEAVY_LLM_PROVIDER", "gemini")
HEAVY_MODEL = cfg("HEAVY_LLM_MODEL", "gemini-3-flash-preview")
HEAVY_URL = cfg("HEAVY_LLM_URL", "https://generativelanguage.googleapis.com/v1beta")

# ULTRA tier: last resort — only when all other tiers fail, very expensive
ULTRA_PROVIDER = cfg("ULTRA_LLM_PROVIDER", "gemini")
ULTRA_MODEL = cfg("ULTRA_LLM_MODEL", "gemini-3.1-pro-preview-customtools")
ULTRA_URL = cfg("ULTRA_LLM_URL", "https://generativelanguage.googleapis.com/v1beta")

# API key for cloud providers
GOOGLE_API_KEY = cfg("GOOGLE_API_KEY", "") or cfg("GEMINI_API_KEY", "") or cfg("LLM_API_KEY", "")

# Legacy compat
LLM_PROVIDER = FAST_PROVIDER
LLM_MODEL = FAST_MODEL
LLM_BASE_URL = FAST_URL
LLM_API_KEY = GOOGLE_API_KEY
LLM_CODE_MODEL = cfg("LLM_CODE_MODEL", FAST_MODEL)

DEFAULT_URLS = {
    "gemini": "https://generativelanguage.googleapis.com/v1beta",
    "openai": "https://api.openai.com/v1",
    "claude": "https://api.anthropic.com/v1",
    "local": "http://localhost:11434/v1",
}

print(f"[PROXY] FAST:   {FAST_PROVIDER}/{FAST_MODEL}")
print(f"[PROXY] MEDIUM: {MEDIUM_PROVIDER}/{MEDIUM_MODEL}")
print(f"[PROXY] HEAVY:  {HEAVY_PROVIDER}/{HEAVY_MODEL}")
print(f"[PROXY] ULTRA:  {ULTRA_PROVIDER}/{ULTRA_MODEL}")
if GOOGLE_API_KEY:
    print(f"[PROXY] Cloud API key: ...{GOOGLE_API_KEY[-8:]}")
else:
    print(f"[PROXY] No cloud API key — cloud tiers will fall back to FAST")

COM2_HOST = "127.0.0.1"
COM2_PORT = 4567

REQ_START = b"@@GEMINI_REQ@@"
REQ_END = b"@@END@@"
RESP_START = b"@@GEMINI_RESP@@"
RESP_END = b"@@END@@"


TOOL_GENERATE_PREFIX = "[GENERATE_TOOL]"
TIME_SYNC_PREFIX = "[TIME_SYNC]"
LOAD_WASM_PREFIX = "[LOAD_WASM:"  # [LOAD_WASM:/path/to/file.wasm]

# Maximum compiled WASM size (64KB) — prevents COM2 buffer overflow
MAX_WASM_SIZE = 64 * 1024

# Complete registry of all folk_* host functions available to WASM tools.
# Used to auto-inject missing extern declarations after LLM code generation.
FOLK_EXTERNS = {
    # Drawing
    "folk_draw_rect": "fn folk_draw_rect(x: i32, y: i32, w: i32, h: i32, color: i32);",
    "folk_draw_line": "fn folk_draw_line(x1: i32, y1: i32, x2: i32, y2: i32, color: i32);",
    "folk_draw_circle": "fn folk_draw_circle(cx: i32, cy: i32, r: i32, color: i32);",
    "folk_draw_text": "fn folk_draw_text(x: i32, y: i32, ptr: i32, len: i32, color: i32);",
    "folk_fill_screen": "fn folk_fill_screen(color: i32);",
    # System Metrics
    "folk_get_time": "fn folk_get_time() -> i32;",
    "folk_screen_width": "fn folk_screen_width() -> i32;",
    "folk_screen_height": "fn folk_screen_height() -> i32;",
    "folk_random": "fn folk_random() -> i32;",
    "folk_get_datetime": "fn folk_get_datetime(ptr: i32) -> i32;",
    # Input
    "folk_poll_event": "fn folk_poll_event(event_ptr: i32) -> i32;",
    # Direct Pixel Access
    "folk_get_surface": "fn folk_get_surface() -> i32;",
    "folk_surface_pitch": "fn folk_surface_pitch() -> i32;",
    "folk_surface_present": "fn folk_surface_present();",
    # Async File
    "folk_request_file": "fn folk_request_file(path_ptr: i32, path_len: i32, dest_ptr: i32, dest_len: i32) -> i32;",
}

import re

def fix_missing_externs(source: str) -> str:
    """Scan for folk_* calls not declared in extern blocks. Inject missing ones."""
    # Find all folk_* function calls in the source
    used = set(re.findall(r'\bfolk_\w+', source))

    # Find which ones are already declared in an extern "C" block
    declared = set()
    for m in re.finditer(r'extern\s+"C"\s*\{([^}]*)\}', source, re.DOTALL):
        block = m.group(1)
        declared.update(re.findall(r'\bfolk_\w+', block))

    # Find missing declarations
    missing = []
    for name in sorted(used - declared):
        if name in FOLK_EXTERNS:
            missing.append(f"    {FOLK_EXTERNS[name]}")

    if not missing:
        return source

    print(f"[SERIAL-PROXY] Auto-injecting {len(missing)} missing extern(s): {', '.join(n for n in sorted(used - declared) if n in FOLK_EXTERNS)}")

    # Strategy: append missing declarations into the FIRST extern "C" block
    first_extern = re.search(r'(extern\s+"C"\s*\{)', source)
    if first_extern:
        insert_pos = first_extern.end()
        injection = "\n" + "\n".join(missing) + "\n"
        source = source[:insert_pos] + injection + source[insert_pos:]
    else:
        # No extern block exists — create one after #![no_main]
        anchor = source.find("#![no_main]")
        if anchor != -1:
            insert_pos = source.index("\n", anchor) + 1
        else:
            insert_pos = 0
        block = 'extern "C" {\n' + "\n".join(missing) + "\n}\n"
        source = source[:insert_pos] + block + source[insert_pos:]

    return source


def fix_unsafe_calls(source: str) -> str:
    """If run() body has bare extern calls (no unsafe block), wrap the body in unsafe { }."""
    # Check if there's already an unsafe block inside run()
    run_match = re.search(r'pub\s+extern\s+"C"\s+fn\s+run\s*\(\s*\)\s*\{', source)
    if not run_match:
        return source

    # Check if unsafe already present after run() opening
    after_run = source[run_match.end():]
    stripped = after_run.lstrip()
    if stripped.startswith("unsafe"):
        return source  # Already has unsafe

    # Check if any folk_ calls exist in the function (indicating extern calls)
    if not re.search(r'\bfolk_\w+\s*\(', after_run.split("\n#")[0]):
        return source  # No extern calls in run()

    # Wrap the body: insert "unsafe {" after opening brace, and "}" before closing brace
    # Find the matching closing brace by counting braces
    depth = 1
    pos = run_match.end()
    while pos < len(source) and depth > 0:
        if source[pos] == '{':
            depth += 1
        elif source[pos] == '}':
            depth -= 1
        pos += 1

    if depth != 0:
        return source  # Can't find matching brace

    closing_pos = pos - 1  # Position of the closing '}'
    insert_after = run_match.end()

    source = (source[:insert_after] + "\n    unsafe {" +
              source[insert_after:closing_pos] + "    }\n" +
              source[closing_pos:])
    print("[SERIAL-PROXY] Auto-wrapped run() body in unsafe { }")
    return source


def fix_infinite_loop(source: str) -> str:
    """Remove infinite loops with spin_loop — WASM tools must be run-to-completion."""
    # Remove `loop { ... core::hint::spin_loop(); }` at the end of run()
    source = re.sub(r'\s*core::hint::spin_loop\(\);\s*', '\n', source)
    # Replace bare `loop {` wrapping the entire run body with just the body
    # This is a common LLM mistake — they use loop {} instead of relying on frame-based calls
    return source


# The Master Prompt — strict no_std WASM code generation
from folkering_context import get_full_wasm_context, get_dream_context

_WASM_CONTEXT = get_full_wasm_context()

WASM_SYSTEM_PROMPT = """You are a code generator for Folkering OS (bare-metal Rust, no_std).
Generate a SINGLE Rust file that compiles to wasm32-unknown-unknown.

""" + _WASM_CONTEXT + """

STRICT RULES:
- #![no_std] and #![no_main] are REQUIRED
- Export: #[no_mangle] pub extern "C" fn run()
- Include: #[panic_handler] fn panic(_: &core::panic::PanicInfo) -> ! { loop {} }
- The ENTIRE body of run() MUST be wrapped in unsafe { } — ALL extern calls require it!
- NO infinite loops (no `loop {}`), NO yielding, NO sleeping — run() is called every frame
- Use `static mut` variables for state that persists between frames (position, velocity, etc)
- Event polling loops (while folk_poll_event() != 0) are OK
- NO println!, NO std, NO allocation, NO extern crate
- Color format: 0x00RRGGBB (alpha channel is IGNORED, use solid colors only)
  Examples: 0x00FF0000 = red, 0x0000FF00 = green, 0x000000FF = blue, 0x00FFFFFF = white

AVAILABLE HOST FUNCTIONS — import only what you need via extern "C" { ... }:

=== Drawing ===
fn folk_draw_rect(x: i32, y: i32, w: i32, h: i32, color: i32);  // filled rectangle
fn folk_draw_line(x1: i32, y1: i32, x2: i32, y2: i32, color: i32);  // line (Bresenham)
fn folk_draw_circle(cx: i32, cy: i32, r: i32, color: i32);  // circle outline (midpoint)
fn folk_draw_text(x: i32, y: i32, ptr: i32, len: i32, color: i32);  // text (ptr to static bytes)
fn folk_fill_screen(color: i32);  // fill entire framebuffer with solid color

=== System Metrics ===
fn folk_get_time() -> i32;       // uptime in milliseconds
fn folk_screen_width() -> i32;   // screen width in pixels (use for self-scaling UI!)
fn folk_screen_height() -> i32;  // screen height in pixels
fn folk_random() -> i32;         // hardware random number (RDRAND)
fn folk_get_datetime(ptr: i32) -> i32;  // writes 6 x i32 (year,month,day,hour,minute,second) at ptr, returns 1 on success

=== Input (Interactive Apps) ===
fn folk_poll_event(event_ptr: i32) -> i32;  // returns event_type (>0) or 0 if none
  // Writes 16-byte FolkEvent at event_ptr: { event_type: i32, x: i32, y: i32, data: i32 }
  // event_type: 1=mouse_move (x,y=absolute pos, data=buttons), 2=mouse_click (x,y=pos, data=button), 3=key_down (data=keycode)

=== Direct Pixel Access (Advanced) ===
fn folk_get_surface() -> i32;       // Returns pixel buffer offset in WASM memory (0 if unavailable)
fn folk_surface_pitch() -> i32;     // Bytes per row (screen_width * 4)
fn folk_surface_present();          // Call AFTER writing all pixels to trigger display

=== Async File Loading ===
fn folk_request_file(path_ptr: i32, path_len: i32, dest_ptr: i32, dest_len: i32) -> i32;
  // Request async file load from VFS. Returns handle (>0) or 0 on error.
  // File data is written to dest_ptr in WASM memory when ready.
  // Check folk_poll_event for event_type=4 (AssetLoaded):
  //   x=handle, y=status (0=ok, 1=not_found), data=bytes_loaded

OUTPUT FORMAT:
Wrap your ENTIRE Rust source code in <TOOL_CODE> tags:
<TOOL_CODE>
#![no_std]
#![no_main]
// ... your code here ...
</TOOL_CODE>

You may think and explain OUTSIDE the tags, but the code MUST be inside <TOOL_CODE>...</TOOL_CODE>.

TIPS:
- Use folk_screen_width()/folk_screen_height() to make UIs that adapt to any resolution
- folk_draw_text ptr must point to static bytes: static TEXT: &[u8] = b"Hello"; then pass TEXT.as_ptr() as i32
- folk_random() returns random i32 — mask with & 0x7FFF for positive values, % N for range
- Negative coordinates are safe (off-screen pixels are clipped automatically)
- You may call the same function multiple times (e.g., draw 20 rectangles)

INTERACTIVE APPS:
- run() is called EVERY FRAME (not just once). Use static mut variables to keep state!
- Call folk_poll_event in a loop at the start of run() to process all pending input
- Use folk_fill_screen to clear before redrawing (prevents ghosting)
- If the user mentions "interactive", "game", "app", "click", "mouse", use this pattern:

```rust
static mut STATE: i32 = 0;  // persists between frames!
#[no_mangle] pub extern "C" fn run() {
    unsafe {
        let mut evt = [0i32; 4];  // [event_type, x, y, data]
        while folk_poll_event(evt.as_mut_ptr() as i32) != 0 {
            if evt[0] == 2 { STATE = evt[1]; }  // mouse click: save x
        }
        folk_fill_screen(0x001a1a2e);
        folk_draw_circle(STATE, 200, 30, 0x00FF0000);
    }
}
```

EXAMPLE — centered circle with screen-adaptive sizing:
```rust
#![no_std]
#![no_main]
extern "C" {
    fn folk_fill_screen(color: i32);
    fn folk_draw_circle(cx: i32, cy: i32, r: i32, color: i32);
    fn folk_draw_text(x: i32, y: i32, ptr: i32, len: i32, color: i32);
    fn folk_screen_width() -> i32;
    fn folk_screen_height() -> i32;
}
#[panic_handler] fn panic(_: &core::panic::PanicInfo) -> ! { loop {} }
static LABEL: &[u8] = b"Folkering OS";
#[no_mangle] pub extern "C" fn run() {
    unsafe {
        let w = folk_screen_width();
        let h = folk_screen_height();
        folk_fill_screen(0x001a1a2e);
        folk_draw_circle(w / 2, h / 2, h / 4, 0x0000FF00);
        folk_draw_text(w / 2 - 48, h / 2 + h / 4 + 20, LABEL.as_ptr() as i32, LABEL.len() as i32, 0x00FFFFFF);
    }
}
```

DIRECT PIXEL EXAMPLE — gradient fill using folk_get_surface:
```rust
#![no_std]
#![no_main]
extern "C" {
    fn folk_get_surface() -> i32;
    fn folk_surface_pitch() -> i32;
    fn folk_surface_present();
    fn folk_screen_width() -> i32;
    fn folk_screen_height() -> i32;
}
#[panic_handler] fn panic(_: &core::panic::PanicInfo) -> ! { loop {} }
#[no_mangle] pub extern "C" fn run() {
    unsafe {
        let surface = folk_get_surface();
        if surface == 0 { return; }
        let w = folk_screen_width();
        let h = folk_screen_height();
        let pitch = folk_surface_pitch() / 4;
        let pixels = surface as *mut u32;
        let mut y = 0;
        while y < h { let mut x = 0; while x < w {
            let r = (x * 255 / w) as u32;
            let b = (y * 255 / h) as u32;
            *pixels.add(y as usize * pitch as usize + x as usize) = (r << 16) | b;
            x += 1; } y += 1; }
        folk_surface_present();
    }
}
```

Output ONLY the Rust code. No explanation, no markdown fences."""


import hashlib

# WASM source cache directory
_WASM_CACHE_DIR = os.path.join(os.path.dirname(os.path.dirname(os.path.abspath(__file__))), "wasm_cache")
os.makedirs(_WASM_CACHE_DIR, exist_ok=True)

def _cache_key(prompt: str) -> str:
    """Generate a filesystem-safe cache key from a prompt."""
    h = hashlib.sha256(prompt.strip().lower().encode()).hexdigest()[:16]
    safe = "".join(c if c.isalnum() or c in "-_" else "_" for c in prompt.strip()[:40])
    return f"{safe}_{h}"

def _cache_check(prompt: str) -> tuple:
    """Check if cached WASM + source exists. Returns (wasm_bytes, source_str) or (None, None)."""
    key = _cache_key(prompt)
    wasm_path = os.path.join(_WASM_CACHE_DIR, f"{key}.wasm")
    src_path = os.path.join(_WASM_CACHE_DIR, f"{key}.rs")
    if os.path.exists(wasm_path):
        with open(wasm_path, "rb") as f:
            wasm = f.read()
        src = ""
        if os.path.exists(src_path):
            with open(src_path, "r") as f:
                src = f.read()
        print(f"[CACHE] Hit: {key} ({len(wasm)} bytes)")
        return wasm, src
    return None, None


def _cache_save_version(prompt: str, source: str, wasm_bytes: bytes):
    """Save a numbered version snapshot (for rollback). Never deleted."""
    key = _cache_key(prompt)
    meta = _cache_get_meta(prompt)
    ver = meta.get("version", 1)
    versions_dir = os.path.join(_WASM_CACHE_DIR, "versions", key)
    os.makedirs(versions_dir, exist_ok=True)
    with open(os.path.join(versions_dir, f"v{ver}.rs"), "w") as f:
        f.write(source)
    with open(os.path.join(versions_dir, f"v{ver}.wasm"), "wb") as f:
        f.write(wasm_bytes)
    with open(os.path.join(versions_dir, f"v{ver}.meta.json"), "w") as f:
        f.write(json.dumps(meta, indent=2))
    print(f"[CACHE] Snapshot saved: {key}/v{ver}")


def _cache_list_versions(prompt: str) -> list:
    """List all saved versions of an app."""
    key = _cache_key(prompt)
    versions_dir = os.path.join(_WASM_CACHE_DIR, "versions", key)
    if not os.path.exists(versions_dir):
        return []
    versions = []
    for f in sorted(os.listdir(versions_dir)):
        if f.endswith(".meta.json"):
            ver_num = f.replace(".meta.json", "").replace("v", "")
            try:
                with open(os.path.join(versions_dir, f), "r") as fh:
                    meta = json.loads(fh.read())
                wasm_path = os.path.join(versions_dir, f"v{ver_num}.wasm")
                size = os.path.getsize(wasm_path) if os.path.exists(wasm_path) else 0
                versions.append({
                    "version": int(ver_num),
                    "id": meta.get("id", "?"),
                    "size": size,
                    "description": meta.get("description", ""),
                    "dream_history": meta.get("dream_history", []),
                })
            except Exception:
                pass
    return versions


def _cache_rollback(prompt: str, target_version: int) -> tuple:
    """Rollback to a specific version. Returns (wasm_bytes, source, error)."""
    key = _cache_key(prompt)
    versions_dir = os.path.join(_WASM_CACHE_DIR, "versions", key)
    src_path = os.path.join(versions_dir, f"v{target_version}.rs")
    wasm_path = os.path.join(versions_dir, f"v{target_version}.wasm")
    meta_path = os.path.join(versions_dir, f"v{target_version}.meta.json")

    if not os.path.exists(wasm_path):
        return None, None, f"Version {target_version} not found"

    with open(wasm_path, "rb") as f:
        wasm = f.read()
    src = ""
    if os.path.exists(src_path):
        with open(src_path, "r") as f:
            src = f.read()

    # Restore as current version
    with open(os.path.join(_WASM_CACHE_DIR, f"{key}.wasm"), "wb") as f:
        f.write(wasm)
    if src:
        with open(os.path.join(_WASM_CACHE_DIR, f"{key}.rs"), "w") as f:
            f.write(src)
    if os.path.exists(meta_path):
        with open(meta_path, "r") as f:
            old_meta = json.loads(f.read())
        import datetime
        old_meta["last_updated"] = datetime.datetime.now().isoformat()
        old_meta["rollback_from"] = _cache_get_meta(prompt).get("version", 0)
        _cache_set_meta(prompt, old_meta)

    print(f"[CACHE] Rolled back {key} to v{target_version} ({len(wasm)} bytes)")
    return wasm, src, None

def _cache_meta_path(prompt: str) -> str:
    return os.path.join(_WASM_CACHE_DIR, f"{_cache_key(prompt)}.meta.json")

def _cache_get_meta(prompt: str) -> dict:
    """Read cache metadata (lineage, strikes, version history)."""
    path = _cache_meta_path(prompt)
    if os.path.exists(path):
        try:
            with open(path, "r") as f:
                return json.loads(f.read())
        except Exception:
            pass
    return {
        "id": "",              # Unique hash of this version's source code
        "parent_id": "",       # ID of the version this was derived from
        "root_id": "",         # ID of the original "genesis" version (shared by all branches)
        "branch": "main",      # Branch name: "main", "fast", "beautiful", etc.
        "strikes": 0,
        "perfected": False,
        "version": 1,
        "description": "",
        "created": "",
        "last_updated": "",
        "lineage": [],         # List of ancestor IDs: [root, ..., parent, self]
        "dream_history": [],   # What dreams have been applied: ["refactor", "creative", ...]
    }

def _cache_set_meta(prompt: str, meta: dict):
    """Write cache metadata."""
    with open(_cache_meta_path(prompt), "w") as f:
        f.write(json.dumps(meta, indent=2))

def _cache_store(prompt: str, source: str, wasm_bytes: bytes, parent_prompt: str = "", dream_mode: str = ""):
    """Store compiled WASM + source code + metadata with lineage tracking."""
    key = _cache_key(prompt)
    with open(os.path.join(_WASM_CACHE_DIR, f"{key}.rs"), "w") as f:
        f.write(source)
    with open(os.path.join(_WASM_CACHE_DIR, f"{key}.wasm"), "wb") as f:
        f.write(wasm_bytes)

    import datetime
    # Unique ID for this version = hash of source code
    source_id = hashlib.sha256(source.encode()).hexdigest()[:12]

    meta = _cache_get_meta(prompt)
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
        parent_meta = _cache_get_meta(parent_prompt)
        meta["parent_id"] = parent_meta.get("id", "")
        meta["root_id"] = parent_meta.get("root_id", "") or parent_meta.get("id", "")
        # Inherit branch name from parent (unless explicitly changed)
        if not meta.get("branch") or meta["branch"] == "main":
            meta["branch"] = parent_meta.get("branch", "main")
        # Build lineage chain
        parent_lineage = parent_meta.get("lineage", [])
        meta["lineage"] = parent_lineage + [parent_meta.get("id", "")]
    else:
        # Genesis: first version of this app
        meta["root_id"] = source_id
        meta["parent_id"] = ""
        meta["lineage"] = []

    # Dream history
    if dream_mode:
        history = meta.get("dream_history", [])
        history.append({"mode": dream_mode, "time": datetime.datetime.now().isoformat(), "version": meta["version"]})
        # Keep last 20 entries
        meta["dream_history"] = history[-20:]

    if not meta.get("created"):
        meta["created"] = datetime.datetime.now().isoformat()
    meta["last_updated"] = datetime.datetime.now().isoformat()
    _cache_set_meta(prompt, meta)

    # Save version snapshot for rollback (never deleted)
    _cache_save_version(prompt, source, wasm_bytes)

    lineage_depth = len(meta.get("lineage", []))
    print(f"[CACHE] Stored: {key} v{meta['version']} id={source_id} depth={lineage_depth} ({len(source)} chars, {len(wasm_bytes)} bytes)")


# ── Lineage Query ────────────────────────────────────────────────────────

def _list_all_apps() -> list:
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


def _get_lineage_tree(prompt: str) -> str:
    """Build a text representation of an app's family tree."""
    meta = _cache_get_meta(prompt)
    if not meta.get("id"):
        return f"No lineage data for '{prompt}'"

    root_id = meta.get("root_id", meta.get("id", ""))
    # Find all apps with same root_id (all relatives)
    family = []
    for app in _list_all_apps():
        if app["root_id"] == root_id or app["id"] == root_id:
            family.append(app)

    if not family:
        return f"'{prompt}' has no known relatives"

    # Sort by version
    family.sort(key=lambda a: a["version"])

    lines = [f"Lineage for '{meta.get('description', prompt)}' (root: {root_id[:8]}...)"]
    lines.append("")
    for app in family:
        prefix = "  " if app["id"] != meta.get("id") else "→ "
        perfected = " [PERFECTED]" if app.get("perfected") else ""
        lines.append(f"{prefix}v{app['version']} [{app['branch']}] id={app['id'][:8]} "
                     f"dreams={app['dreams']}{perfected} — {app['description'][:40]}")

    return "\n".join(lines)


# ── Daily Dream Budget ──────────────────────────────────────────────────
# Prevents AutoDream from burning through API credits overnight.

DREAM_MAX_PER_DAY = int(cfg("DREAM_MAX_CALLS_PER_DAY", "10"))
_DREAM_BUDGET_PATH = os.path.join(_WASM_CACHE_DIR, "dream_budget.json")

def _dream_budget_check() -> bool:
    """Check if today's dream budget is available. Returns True if allowed."""
    import datetime
    today = datetime.date.today().isoformat()
    budget = {"date": "", "calls": 0}
    if os.path.exists(_DREAM_BUDGET_PATH):
        try:
            with open(_DREAM_BUDGET_PATH, "r") as f:
                budget = json.loads(f.read())
        except Exception:
            pass
    # Reset counter if new day
    if budget.get("date") != today:
        budget = {"date": today, "calls": 0}
    if budget["calls"] >= DREAM_MAX_PER_DAY:
        print(f"[DREAM-BUDGET] Blocked: {budget['calls']}/{DREAM_MAX_PER_DAY} calls used today")
        return False
    return True

def _dream_budget_record():
    """Record a dream API call."""
    import datetime
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


def _clarify_request(prompt: str) -> tuple:
    """Ask FAST LLM to clarify an ambiguous generation request.
    Returns (should_proceed: bool, message: str).
    If should_proceed is False, message contains a question or suggestion."""

    # Skip clarification for tweaks, dreams, and very specific prompts
    if "--tweak" in prompt or len(prompt.split()) > 8:
        return True, ""

    # Check if cached variants exist
    all_apps = _list_all_apps()
    # Clean prompt for matching
    clean = prompt.lower()
    for pfx in ["gemini generate ", "generate ", "agent generate "]:
        if clean.startswith(pfx):
            clean = clean[len(pfx):]
            break

    # Find similar cached apps
    similar = [a for a in all_apps if clean in a["description"].lower() or a["description"].lower() in clean]
    if similar:
        suggestions = ", ".join(f"'{a['description']}' (v{a['version']})" for a in similar[:5])
        return False, f"EXISTING: Found similar apps: {suggestions}. Type the exact name to load, or add details to create a new variant."

    # Ask FAST LLM: is this request clear enough to generate code?
    clarify_prompt = (
        f"A user wants to generate a visual WASM app with this description: \"{clean}\"\n\n"
        f"Is this description clear enough to write code? Answer with ONLY one JSON:\n"
        f"If clear: {{\"clear\": true}}\n"
        f"If unclear or nonsensical: {{\"clear\": false, \"question\": \"your clarifying question\"}}\n"
        f"If it could have variants: {{\"clear\": true, \"variants\": [\"simple version\", \"advanced version\"]}}\n"
    )
    try:
        result = call_llm(clarify_prompt, tier="fast")
        result = result.strip()
        # Find JSON in response
        if '{' in result:
            json_str = result[result.index('{'):result.rindex('}') + 1]
            data = json.loads(json_str)
            if not data.get("clear", True):
                return False, f"QUESTION: {data.get('question', 'Could you be more specific?')}"
            if data.get("variants"):
                variants = ", ".join(f"'{v}'" for v in data["variants"][:4])
                return False, f"VARIANTS: Did you mean one of these? {variants}. Be more specific or just press Enter to use the first option."
    except Exception:
        pass  # Clarification failed — proceed anyway

    return True, ""


def _llm_to_wasm(prompt: str, force: bool = False, tweak: str = "") -> tuple:
    """Shared WASM pipeline: clarify → cache check → LLM → extract → fix → compile → cache store.
    Returns (bytes, None) on success, (None, str) on failure.
    Returns (None, "CLARIFY:...") if the request needs user clarification."""

    # Phase 0: Clarification (only for new generation, not tweaks/dreams)
    if not force and not tweak:
        should_proceed, clarify_msg = _clarify_request(prompt)
        if not should_proceed:
            return None, f"CLARIFY:{clarify_msg}"

    # Phase 1: Check cache first (unless --force)
    if not force and not tweak:
        cached_wasm, _ = _cache_check(prompt)
        if cached_wasm:
            return cached_wasm, None

    # If --tweak, load existing source and ask LLM to modify it
    if tweak:
        _, existing_src = _cache_check(prompt)
        # Get app description from metadata (clean, no command prefixes)
        meta = _cache_get_meta(prompt)
        app_desc = meta.get("description", prompt)
        # Extra safety: strip command prefixes from description
        for pfx in ["gemini generate ", "gemini gen ", "generate ", "agent generate ", "--tweak "]:
            if app_desc.lower().startswith(pfx):
                app_desc = app_desc[len(pfx):]
        # Strip tweak quotes if present
        if app_desc.startswith('"') and '" ' in app_desc:
            app_desc = app_desc[app_desc.index('" ') + 2:]

        # Detect dream mode from tweak text for context injection
        if "refactor" in tweak.lower() or "fewer cpu" in tweak.lower():
            dream_ctx = get_dream_context("refactor")
        elif "visual" in tweak.lower() or "improvement" in tweak.lower():
            dream_ctx = get_dream_context("creative")
        elif "harden" in tweak.lower() or "edge case" in tweak.lower():
            dream_ctx = get_dream_context("nightmare")
        else:
            dream_ctx = ""

        if existing_src:
            full_prompt = (f"{WASM_SYSTEM_PROMPT}\n\n{dream_ctx}\n\n"
                          f"APP: \"{app_desc}\"\n"
                          f"This is a visual WASM widget for Folkering OS. "
                          f"It must remain a {app_desc} after your changes.\n\n"
                          f"TECHNICAL REMINDER:\n"
                          f"- NO crate imports (no `use`, no `extern crate`)\n"
                          f"- ONLY the folk_* host functions listed above\n"
                          f"- Must compile with: cargo build --target wasm32-unknown-unknown\n"
                          f"- #![no_std] #![no_main] are REQUIRED\n\n"
                          f"Current source code:\n```rust\n{existing_src}\n```\n\n"
                          f"YOUR TASK: {tweak}")
        else:
            full_prompt = f"{WASM_SYSTEM_PROMPT}\n\nGenerate: {prompt}\nAlso apply this tweak: {tweak}"
    else:
        # Clean the prompt for generation
        clean = prompt
        for pfx in ["gemini generate ", "gemini gen ", "generate "]:
            if clean.lower().startswith(pfx):
                clean = clean[len(pfx):]
                break
        full_prompt = f"{WASM_SYSTEM_PROMPT}\n\nGenerate a WASM app: {clean}"

    print(f"[WASM] Generating code via MEDIUM tier...")
    source = call_llm(full_prompt, tier="medium")

    if source.startswith("Error:"):
        return None, source

    # Extract code from LLM response
    if "<think>" in source and "</think>" in source:
        source = source[source.index("</think>") + 8:]
    extracted = None
    if "<TOOL_CODE>" in source and "</TOOL_CODE>" in source:
        extracted = source.split("<TOOL_CODE>")[1].split("</TOOL_CODE>")[0]
    elif "```rust" in source:
        extracted = source.split("```rust")[1].split("```")[0]
    elif "```" in source:
        parts = source.split("```")
        if len(parts) >= 3: extracted = parts[1]
    if extracted: source = extracted.strip()
    elif "#![no_std]" in source: source = source[source.index("#![no_std]"):].strip()
    else: source = source.strip()

    # Fix common LLM mistakes
    source = fix_missing_externs(source)
    source = fix_unsafe_calls(source)
    source = fix_infinite_loop(source)
    print(f"[WASM] Source: {len(source)} chars")

    # Compile
    tmp_dir = tempfile.mkdtemp(prefix="folkwasm_")
    try:
        proj_dir = os.path.join(tmp_dir, "wasm_tool")
        subprocess.run(["cargo", "new", "--lib", proj_dir], capture_output=True, timeout=10)
        with open(os.path.join(proj_dir, "Cargo.toml"), "w") as f:
            f.write('[package]\nname = "wasm_tool"\nversion = "0.1.0"\nedition = "2021"\n'
                    '[lib]\ncrate-type = ["cdylib"]\n[profile.release]\n'
                    'opt-level = "z"\nlto = true\nstrip = true\npanic = "abort"\n')
        with open(os.path.join(proj_dir, "src", "lib.rs"), "w") as f:
            f.write(source)
        result = subprocess.run(
            ["cargo", "build", "--target", "wasm32-unknown-unknown", "--release"],
            capture_output=True, text=True, timeout=60, cwd=proj_dir)
        if result.returncode != 0:
            return None, f"Compile error: {result.stderr[:400]}"
        wasm_path = os.path.join(proj_dir, "target", "wasm32-unknown-unknown", "release", "wasm_tool.wasm")
        if not os.path.exists(wasm_path):
            return None, "WASM output not found"
        with open(wasm_path, "rb") as f:
            wasm_binary = f.read()
        if len(wasm_binary) > MAX_WASM_SIZE:
            return None, f"WASM too large: {len(wasm_binary)} bytes"
        print(f"[WASM] Compiled: {len(wasm_binary)} bytes")
        # Detect dream mode from tweak text for lineage
        dream_mode_tag = ""
        parent = ""
        if tweak:
            parent = prompt  # The original app is the parent
            if "refactor" in tweak.lower(): dream_mode_tag = "refactor"
            elif "visual" in tweak.lower() or "improvement" in tweak.lower(): dream_mode_tag = "creative"
            elif "harden" in tweak.lower(): dream_mode_tag = "nightmare"
            else: dream_mode_tag = "tweak"
        _cache_store(prompt, source, wasm_binary, parent_prompt=parent, dream_mode=dream_mode_tag)
        return wasm_binary, None
    except subprocess.TimeoutExpired:
        return None, "Compile timeout (60s)"
    except Exception as e:
        return None, f"Build error: {e}"
    finally:
        shutil.rmtree(tmp_dir, ignore_errors=True)


def generate_and_compile_wasm(prompt: str, sock: socket.socket):
    """Legacy path: Generate WASM and send via @@GEMINI_RESP@@ protocol."""
    print(f"[SERIAL-PROXY] Tool request (legacy): {prompt[:80]}...")
    wasm_binary, error = _llm_to_wasm(prompt)
    if error:
        error_resp = RESP_START + error.encode() + RESP_END + b"\n"
        sock.sendall(error_resp)
        print(f"[SERIAL-PROXY] Error: {error[:100]}")
        return
    b64_data = base64.b64encode(wasm_binary).decode("ascii")
    resp_json = json.dumps({"action": "tool_ready", "binary": b64_data})
    response = RESP_START + resp_json.encode() + RESP_END + b"\n"
    sock.sendall(response)
    print(f"[SERIAL-PROXY] Sent tool_ready: {len(wasm_binary)} bytes WASM")


def _dispatch(provider: str, url: str, model: str, prompt: str) -> str:
    """Route to the correct provider backend."""
    if provider == "gemini":
        return _call_gemini(prompt, model, url)
    elif provider == "local":
        return _call_local(prompt, model, url)
    elif provider == "openai":
        return _call_openai(prompt, model, url)
    elif provider == "claude":
        return _call_claude(prompt, model)
    else:
        return f"Error: Unknown provider '{provider}'"


_TIER_CHAIN = [
    ("fast",   lambda: (FAST_PROVIDER,   FAST_MODEL,   FAST_URL)),
    ("medium", lambda: (MEDIUM_PROVIDER, MEDIUM_MODEL, MEDIUM_URL)),
    ("heavy",  lambda: (HEAVY_PROVIDER,  HEAVY_MODEL,  HEAVY_URL)),
    ("ultra",  lambda: (ULTRA_PROVIDER,  ULTRA_MODEL,  ULTRA_URL)),
]

# Ultra tier rate limiter — prevents runaway costs
ULTRA_MAX_PER_SESSION = 3          # max ultra calls per proxy session
ULTRA_COOLDOWN_S = 300             # 5 min cooldown between ultra calls
_ultra_count = 0
_ultra_last_ts = 0.0

def _ultra_allowed() -> bool:
    """Check if ultra tier is allowed right now."""
    global _ultra_count, _ultra_last_ts
    if _ultra_count >= ULTRA_MAX_PER_SESSION:
        print(f"[ROUTER] ULTRA blocked: {_ultra_count}/{ULTRA_MAX_PER_SESSION} calls used this session")
        return False
    now = time.time()
    if _ultra_last_ts > 0 and now - _ultra_last_ts < ULTRA_COOLDOWN_S:
        remaining = int(ULTRA_COOLDOWN_S - (now - _ultra_last_ts))
        print(f"[ROUTER] ULTRA blocked: cooldown ({remaining}s remaining)")
        return False
    return True

def _ultra_record():
    """Record an ultra tier call."""
    global _ultra_count, _ultra_last_ts
    _ultra_count += 1
    _ultra_last_ts = time.time()
    print(f"[ROUTER] ULTRA used: {_ultra_count}/{ULTRA_MAX_PER_SESSION} this session")


def call_llm(prompt: str, model_override: str = "", tier: str = "fast") -> str:
    """Call LLM using the hybrid router with auto-escalation on failure.

    Tiers: fast → medium → heavy → ultra
    On failure, escalates to the next tier automatically.
    Ultra is rate-limited: max 3 calls per session, 5 min cooldown.
    """
    tier_names = [t[0] for t in _TIER_CHAIN]
    start_idx = tier_names.index(tier) if tier in tier_names else 0

    for i in range(start_idx, len(_TIER_CHAIN)):
        tier_name, get_config = _TIER_CHAIN[i]

        # Ultra rate limiter
        if tier_name == "ultra" and not _ultra_allowed():
            return "Error: ultra tier rate-limited (max 3/session, 5min cooldown)"

        provider, model, url = get_config()

        if model_override and i == start_idx:
            model = model_override

        # Skip cloud tiers without API key
        if provider != "local" and not GOOGLE_API_KEY:
            if i == start_idx:
                print(f"[ROUTER] No API key for {tier_name}/{provider}, falling back to FAST")
            provider, model, url = FAST_PROVIDER, FAST_MODEL, FAST_URL

        print(f"[ROUTER] tier={tier_name} -> {provider}/{model}")
        try:
            result = _dispatch(provider, url, model, prompt)
            if result and not result.startswith("Error:"):
                if tier_name == "ultra":
                    _ultra_record()
                return result
            raise ValueError(result)
        except Exception as e:
            if i < len(_TIER_CHAIN) - 1:
                print(f"[ROUTER] {tier_name} failed ({e}), escalating...")
            else:
                return f"Error: all tiers exhausted ({e})"

    return "Error: no tiers available"


def route_for_task(msg_type: str, prompt: str = "") -> str:
    """Decide which tier to use based on task type and prompt content.

    Returns: 'fast', 'medium', or 'heavy'
    """
    # Draug background analysis — always fast (cheap, non-critical)
    if "draug" in prompt.lower() or "background daemon" in prompt.lower():
        return "fast"

    # WASM code generation — medium (needs decent code output)
    if msg_type == "wasm_gen_request" or "generate_wasm" in prompt.lower():
        return "medium"

    # Agent tool calls — medium (needs structured JSON output)
    if msg_type == "chat_request" and ("tool" in prompt.lower() or "agent" in prompt.lower()):
        return "medium"

    # Complex multi-step reasoning (long prompts with history)
    if len(prompt) > 4000:
        return "medium"

    # Default: fast
    return "fast"


def _call_gemini(prompt: str, model: str = "", base_url: str = "") -> str:
    """Google Gemini API."""
    m = model or MEDIUM_MODEL
    base = base_url or MEDIUM_URL
    url = f"{base}/models/{m}:generateContent?key={GOOGLE_API_KEY}"
    body = json.dumps({
        "contents": [{"parts": [{"text": prompt}]}]
    }).encode()
    req = urllib.request.Request(url, data=body,
        headers={"Content-Type": "application/json"}, method="POST")
    ctx = ssl.create_default_context()
    with urllib.request.urlopen(req, context=ctx, timeout=60) as resp:
        result = json.loads(resp.read())
        return result["candidates"][0]["content"]["parts"][0]["text"]


def _call_openai(prompt: str, model: str = "", base_url: str = "") -> str:
    """OpenAI-compatible API (OpenAI, LM Studio, Ollama, llama.cpp server)."""
    m = model or FAST_MODEL
    base = base_url or FAST_URL
    url = f"{base}/chat/completions"
    body = json.dumps({
        "model": m,
        "messages": [{"role": "user", "content": prompt}],
        "max_tokens": 4096,
        "temperature": 0.7,
    }).encode()
    headers = {"Content-Type": "application/json"}
    if LLM_API_KEY:
        headers["Authorization"] = f"Bearer {LLM_API_KEY}"
    req = urllib.request.Request(url, data=body, headers=headers, method="POST")
    # Use SSL only for HTTPS URLs
    kwargs = {"timeout": 120}
    if url.startswith("https"):
        kwargs["context"] = ssl.create_default_context()
    with urllib.request.urlopen(req, **kwargs) as resp:
        result = json.loads(resp.read())
        return result["choices"][0]["message"]["content"]


def _call_local(prompt: str, model: str = "", base_url: str = "") -> str:
    """Local Ollama API — uses native /api/chat to capture <think> reasoning."""
    m = model or FAST_MODEL
    # Use Ollama native API (not OpenAI compat) to get 'thinking' field
    raw_url = base_url or FAST_URL
    base = raw_url.rstrip("/v1").rstrip("/")  # http://localhost:11434
    url = f"{base}/api/chat"
    body = json.dumps({
        "model": m,
        "messages": [{"role": "user", "content": prompt}],
        "stream": False,
    }).encode()
    req = urllib.request.Request(url, data=body,
        headers={"Content-Type": "application/json"}, method="POST")
    with urllib.request.urlopen(req, timeout=120) as resp:
        result = json.loads(resp.read())
        msg = result.get("message", {})
        content = msg.get("content", "")
        thinking = msg.get("thinking", "")
        # Debug: log thinking status
        print(f"[PROXY] model={m}, thinking: {len(thinking)} chars, content: {len(content)} chars")
        # Wrap thinking in <think> tags for Folkering OS FSA parser
        if thinking:
            return f"<think>\n{thinking}\n</think>\n{content}"
        return content


def _call_claude(prompt: str, model: str = "") -> str:
    """Anthropic Claude API."""
    m = model or LLM_MODEL
    url = f"{LLM_BASE_URL}/messages"
    body = json.dumps({
        "model": m,
        "max_tokens": 4096,
        "messages": [{"role": "user", "content": prompt}],
    }).encode()
    req = urllib.request.Request(url, data=body, headers={
        "Content-Type": "application/json",
        "x-api-key": LLM_API_KEY,
        "anthropic-version": "2023-06-01",
    }, method="POST")
    ctx = ssl.create_default_context()
    with urllib.request.urlopen(req, context=ctx, timeout=60) as resp:
        result = json.loads(resp.read())
        return result["content"][0]["text"]


def handle_mcp_frame(frame_bytes: bytes, sock: socket.socket):
    """Process a complete MCP frame (COBS-encoded, CRC-verified, Postcard-serialized)."""
    from mcp_bridge import (parse_frame, decode_mcp_response, make_frame, send_reliable,
        encode_chat_response, encode_time_sync, encode_wasm_chunk, encode_ping,
        _retx_queue, _session, send_wasm_chunked)

    try:
        sid, seq, payload = parse_frame(frame_bytes)
        msg = decode_mcp_response(payload)
    except Exception:
        # Silently drop unparseable frames (ACK/NACK are tiny, may fragment)
        return

    # Session lock: first frame locks the session, subsequent frames are validated
    if not _session.locked:
        _session.lock_to(sid)
        print(f"[MCP] Session locked to 0x{sid:08X}")
    elif not _session.validate(sid):
        print(f"[MCP] DROPPED: wrong session 0x{sid:08X} (expected 0x{_session.session_id:08X})")
        return

    msg_type = msg.get('type', 'unknown')
    print(f"[MCP] Received: {msg_type} (seq={seq})")

    if msg_type == 'chat_request':
        prompt = msg['prompt']

        # Handle internal commands (not LLM calls)
        if prompt.startswith("__ROLLBACK__"):
            parts = prompt.split()
            if len(parts) >= 3:
                app_name = parts[1]
                try:
                    ver = int(parts[2])
                    wasm, src, err = _cache_rollback(app_name, ver)
                    if err:
                        response_frame = make_frame(encode_chat_response(f"Rollback failed: {err}"))
                    else:
                        response_frame = make_frame(encode_chat_response(
                            f"Rolled back '{app_name}' to v{ver} ({len(wasm)} bytes). Restart app to see changes."))
                        # Also send the WASM binary so OS can update its cache
                        send_wasm_chunked(wasm, sock, session_id=_session.session_id)
                except Exception as e:
                    response_frame = make_frame(encode_chat_response(f"Rollback error: {e}"))
            else:
                response_frame = make_frame(encode_chat_response("Usage: __ROLLBACK__ <app_name> <version>"))
            sock.sendall(response_frame)
            return

        # Normal LLM routing
        tier = route_for_task(msg_type, prompt)
        print(f"[MCP] Chat ({tier}): {prompt[:80]}...")

        # Context management: check compaction thresholds
        if _context_mgr.needs_full_compact():
            print(f"[CTX] FULL COMPACT ({_context_mgr.usage_pct():.0f}%)")
            _context_mgr.full_compact()
        elif _context_mgr.needs_auto_compact():
            print(f"[CTX] AUTO COMPACT ({_context_mgr.usage_pct():.0f}%)")
            _context_mgr.auto_compact(lambda text: call_llm(text, tier="fast"))

        _context_mgr.add_message("user", prompt)
        full_context = _context_mgr.get_prompt_text()
        response_text = call_llm(full_context, tier=tier)

        # Strip <think>...</think> tags to reduce wire size
        clean_text = response_text
        if "<think>" in clean_text and "</think>" in clean_text:
            think_end = clean_text.index("</think>") + len("</think>")
            clean_text = clean_text[think_end:].strip()
            print(f"[MCP] Think stripped: {len(response_text)} -> {len(clean_text)} chars")
        _context_mgr.add_message("assistant", clean_text)

        # Send MCP ChatResponse reliably (enqueued for retransmission)
        payload = encode_chat_response(clean_text)
        seq = send_reliable(payload, sock, session_id=_session.session_id)
        print(f"[MCP] Sent ChatResponse seq={seq}: {len(clean_text)} chars (ctx: {_context_mgr.usage_pct():.0f}%)")

    elif msg_type == 'time_sync_request':
        import datetime
        now_local = datetime.datetime.now()
        utc_offset = datetime.datetime.now(datetime.timezone.utc).astimezone().utcoffset()
        offset_minutes = int(utc_offset.total_seconds() / 60) if utc_offset else 0
        offset_minutes = round(offset_minutes / 15) * 15
        payload = encode_time_sync(
            now_local.year, now_local.month, now_local.day,
            now_local.hour, now_local.minute, now_local.second,
            offset_minutes
        )
        seq = send_reliable(payload, sock, session_id=_session.session_id)
        print(f"[MCP] Sent TimeSync seq={seq}: {now_local.hour}:{now_local.minute:02d} UTC+{offset_minutes//60}")

    elif msg_type == 'wasm_gen_request':
        desc = msg.get('description', '')
        is_dream = "--tweak" in desc and ("refactor" in desc.lower() or "optimize" in desc.lower() or "visual" in desc.lower())
        print(f"[MCP] WASM gen{'(dream)' if is_dream else ''}: {desc[:60]}...")

        # Dream budget check — reject if daily quota exhausted
        if is_dream and not _dream_budget_check():
            error_frame = make_frame(encode_chat_response("Error: dream budget exhausted for today"))
            sock.sendall(error_frame)
        else:
            # Check if app is "perfected" (three strikes) — skip refactor dreams
            base_key = desc.rsplit(' ', 1)[-1] if ' ' in desc else desc
            meta = _cache_get_meta(base_key)
            if is_dream and meta.get("perfected") and "refactor" in desc.lower():
                error_frame = make_frame(encode_chat_response("Error: app perfected — skipping refactor"))
                sock.sendall(error_frame)
                print(f"[CACHE] Skipped perfected app: {base_key}")
            else:
                wasm_binary, error = _llm_to_wasm(desc)
                if error and error.startswith("CLARIFY:"):
                    # Clarification needed — send question back as ChatResponse
                    clarify_msg = error[8:]  # Strip "CLARIFY:" prefix
                    print(f"[MCP] Clarification needed: {clarify_msg[:60]}")
                    response_frame = make_frame(encode_chat_response(clarify_msg))
                    sock.sendall(response_frame)
                elif error:
                    error_frame = make_frame(encode_chat_response(f"Error: {error}"))
                    sock.sendall(error_frame)
                else:
                    if is_dream:
                        _dream_budget_record()
                    send_wasm_chunked(wasm_binary, sock, session_id=_session.session_id)

    elif msg_type == 'pong':
        print(f"[MCP] Pong seq={msg.get('seq', 0)}")

    elif msg_type == 'ack':
        # OS acknowledged our frame — clear from retransmission queue
        _retx_queue.on_ack(seq)
        # Don't print for every ACK (too noisy)

    elif msg_type == 'nack':
        reason = msg.get('reason', 0)
        reasons = {1: 'CRC', 2: 'PARSE', 3: 'SESSION', 4: 'CHUNK_ORDER'}
        print(f"[MCP] NACK seq={seq} reason={reasons.get(reason, reason)}")
        _retx_queue.on_nack(seq, sock)

    elif msg_type == 'sampling_request':
        prompt = msg['prompt']
        max_tokens = msg.get('max_tokens', 4096)
        print(f"[MCP] Sampling: {prompt[:60]}... (max {max_tokens})")
        response_text = call_llm(prompt)
        response_frame = make_frame(encode_chat_response(response_text))
        sock.sendall(response_frame)

    else:
        print(f"[MCP] Unknown message type: {msg}")


def handle_serial(sock: socket.socket):
    """Read from COM2, process requests, write responses.
    Dual-mode: supports both MCP (COBS frames with 0x00 sentinel) and
    legacy (@@GEMINI_REQ@@...@@END@@) protocols simultaneously."""
    buf = b""
    # Reset session on new connection (OS may have rebooted)
    from mcp_bridge import _session as bridge_session
    bridge_session.locked = False
    bridge_session.session_id = 0
    bridge_session.seq_counter = 0
    print("[SERIAL-PROXY] Connected to COM2 (dual-mode: MCP + legacy, session reset)...")

    while True:
        try:
            data = sock.recv(4096)
            if not data:
                print("[SERIAL-PROXY] COM2 disconnected")
                break

            buf += data

            # === MCP Protocol: check for COBS frames (0x00 sentinel) ===
            while b'\x00' in buf:
                sentinel_pos = buf.index(b'\x00')
                if sentinel_pos > 0:
                    frame = buf[:sentinel_pos]
                    buf = buf[sentinel_pos + 1:]
                    # Verify this looks like a COBS frame (no 0x00 inside)
                    if b'\x00' not in frame and len(frame) >= 3:
                        try:
                            handle_mcp_frame(frame, sock)
                        except Exception as e:
                            # Silently drop malformed frames (ACKs, partial data)
                            pass
                else:
                    # Leading 0x00 — skip it
                    buf = buf[1:]

            # === Legacy Protocol: @@GEMINI_REQ@@...@@END@@ ===
            while REQ_START in buf and REQ_END in buf:
                start = buf.index(REQ_START) + len(REQ_START)
                end = buf.index(REQ_END)

                if start > end:
                    # Malformed — skip
                    buf = buf[end + len(REQ_END):]
                    continue

                payload = buf[start:end]
                buf = buf[end + len(REQ_END):]

                print(f"[SERIAL-PROXY] Request: {payload[:100]}...")

                # Parse JSON
                try:
                    data = json.loads(payload)
                    prompt = data.get("prompt", payload.decode("utf-8", errors="replace"))
                except json.JSONDecodeError:
                    prompt = payload.decode("utf-8", errors="replace")

                # Check for [TIME_SYNC] prefix → return host local time
                if prompt.startswith(TIME_SYNC_PREFIX):
                    import datetime
                    now = datetime.datetime.now(datetime.timezone.utc).astimezone()
                    utc_offset = now.utcoffset()
                    offset_minutes = int(utc_offset.total_seconds() / 60) if utc_offset else 0
                    # Round to nearest 15 min (all real timezones are multiples of 15/30/45/60)
                    offset_minutes = round(offset_minutes / 15) * 15
                    is_dst = bool(time.daylight and time.localtime().tm_isdst)
                    tz_name = time.tzname[1] if is_dst else time.tzname[0]
                    # Use timezone-aware local time
                    now_local = datetime.datetime.now()
                    time_data = json.dumps({
                        "year": now_local.year, "month": now_local.month, "day": now_local.day,
                        "hour": now_local.hour, "minute": now_local.minute, "second": now_local.second,
                        "utc_offset_minutes": offset_minutes,
                        "tz": tz_name, "dst": is_dst,
                    })
                    response = RESP_START + time_data.encode() + RESP_END + b"\n"
                    sock.sendall(response)
                    print(f"[SERIAL-PROXY] Time sync: {time_data}")
                    continue

                # Check for [LOAD_WASM:path] → load precompiled WASM from host disk
                if prompt.startswith(LOAD_WASM_PREFIX):
                    wasm_path = prompt[len(LOAD_WASM_PREFIX):].rstrip("]").strip()
                    print(f"[SERIAL-PROXY] Loading WASM from: {wasm_path}")
                    try:
                        with open(wasm_path, "rb") as wf:
                            wasm_binary = wf.read()
                        if len(wasm_binary) > MAX_WASM_SIZE:
                            error = f"WASM too large: {len(wasm_binary)} bytes"
                            sock.sendall(RESP_START + error.encode() + RESP_END + b"\n")
                        else:
                            b64_data = base64.b64encode(wasm_binary).decode("ascii")
                            resp_json = json.dumps({"action": "tool_ready", "binary": b64_data})
                            sock.sendall(RESP_START + resp_json.encode() + RESP_END + b"\n")
                            print(f"[SERIAL-PROXY] Loaded {len(wasm_binary)} bytes WASM")
                    except FileNotFoundError:
                        sock.sendall(RESP_START + f"File not found: {wasm_path}".encode() + RESP_END + b"\n")
                    except Exception as e:
                        sock.sendall(RESP_START + f"Load error: {e}".encode() + RESP_END + b"\n")
                    continue

                # Check for [GENERATE_TOOL] prefix → WASM pipeline
                if prompt.startswith(TOOL_GENERATE_PREFIX):
                    tool_prompt = prompt[len(TOOL_GENERATE_PREFIX):].strip()
                    generate_and_compile_wasm(tool_prompt, sock)
                    continue

                # Regular Gemini query
                print(f"[SERIAL-PROXY] Calling {LLM_PROVIDER}...")
                response_text = call_llm(prompt)
                print(f"[SERIAL-PROXY] Response: {len(response_text)} chars")

                # Send response back via COM2
                response = RESP_START + response_text.encode("utf-8", errors="replace") + RESP_END + b"\n"
                sock.sendall(response)
                print(f"[SERIAL-PROXY] Sent {len(response)} bytes back to OS")

        except socket.timeout:
            # Check retransmission timeouts on every socket timeout (1s)
            if not _retx_queue.is_empty():
                _retx_queue.check_timeouts(sock)
            continue
        except Exception as e:
            print(f"[SERIAL-PROXY] Error: {e}")
            break


def main():
    print(f"[SERIAL-PROXY] Connecting to QEMU COM2 at {COM2_HOST}:{COM2_PORT}...")

    while True:
        try:
            sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
            sock.connect((COM2_HOST, COM2_PORT))
            sock.settimeout(1.0)
            handle_serial(sock)
        except ConnectionRefusedError:
            print("[SERIAL-PROXY] COM2 not available yet, retrying in 2s...")
            time.sleep(2)
        except Exception as e:
            print(f"[SERIAL-PROXY] Connection error: {e}, retrying in 2s...")
            time.sleep(2)
        finally:
            try:
                sock.close()
            except:
                pass


if __name__ == "__main__":
    main()
