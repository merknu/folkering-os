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

# Provider: "gemini", "openai", or "claude"
LLM_PROVIDER = cfg("LLM_PROVIDER", "gemini")
LLM_API_KEY = cfg("LLM_API_KEY", "") or cfg("GEMINI_API_KEY", "")
LLM_MODEL = cfg("LLM_MODEL", "")
LLM_BASE_URL = cfg("LLM_BASE_URL", "")

if not LLM_API_KEY:
    print("[PROXY] ERROR: Set LLM_API_KEY (or GEMINI_API_KEY) in .env or environment")
    sys.exit(1)

# Default models per provider
DEFAULT_MODELS = {
    "gemini": "gemini-3.1-flash-lite-preview",
    "openai": "gpt-4o-mini",
    "claude": "claude-sonnet-4-20250514",
}
if not LLM_MODEL:
    LLM_MODEL = DEFAULT_MODELS.get(LLM_PROVIDER, "gemini-2.5-flash")

# Default base URLs per provider
DEFAULT_URLS = {
    "gemini": "https://generativelanguage.googleapis.com/v1beta",
    "openai": "https://api.openai.com/v1",
    "claude": "https://api.anthropic.com/v1",
}
if not LLM_BASE_URL:
    LLM_BASE_URL = DEFAULT_URLS.get(LLM_PROVIDER, DEFAULT_URLS["gemini"])

print(f"[PROXY] Provider: {LLM_PROVIDER} | Model: {LLM_MODEL} | URL: {LLM_BASE_URL}")

COM2_HOST = "127.0.0.1"
COM2_PORT = 4567

REQ_START = b"@@GEMINI_REQ@@"
REQ_END = b"@@END@@"
RESP_START = b"@@GEMINI_RESP@@"
RESP_END = b"@@END@@"


TOOL_GENERATE_PREFIX = "[GENERATE_TOOL]"

# Maximum compiled WASM size (64KB) — prevents COM2 buffer overflow
MAX_WASM_SIZE = 64 * 1024

# The Master Prompt — strict no_std WASM code generation
WASM_SYSTEM_PROMPT = """You are a code generator for Folkering OS (bare-metal Rust, no_std).
Generate a SINGLE Rust file that compiles to wasm32-unknown-unknown.

STRICT RULES:
- #![no_std] and #![no_main] are REQUIRED
- Export: #[no_mangle] pub extern "C" fn run()
- Include: #[panic_handler] fn panic(_: &core::panic::PanicInfo) -> ! { loop {} }
- NO infinite loops, NO yielding, NO sleeping — run-to-completion per frame
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

=== Input (Interactive Apps) ===
fn folk_poll_event(event_ptr: i32) -> i32;  // returns event_type (>0) or 0 if none
  // Writes 16-byte FolkEvent at event_ptr: { event_type: i32, x: i32, y: i32, data: i32 }
  // event_type: 1=mouse_move (x,y=absolute pos, data=buttons), 2=mouse_click (x,y=pos, data=button), 3=key_down (data=keycode)

=== Direct Pixel Access (Advanced) ===
fn folk_get_surface() -> i32;       // Returns pixel buffer offset in WASM memory (0 if unavailable)
fn folk_surface_pitch() -> i32;     // Bytes per row (screen_width * 4)
fn folk_surface_present();          // Call AFTER writing all pixels to trigger display

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


def generate_and_compile_wasm(prompt: str, sock: socket.socket):
    """Generate Rust code via Gemini, compile to WASM, send base64 binary back."""
    print(f"[SERIAL-PROXY] Tool request: {prompt[:80]}...")

    # Step 1: Call Gemini with code-gen system prompt
    full_prompt = f"{WASM_SYSTEM_PROMPT}\n\nGenerate: {prompt}"
    print(f"[SERIAL-PROXY] Calling {LLM_PROVIDER} for code generation...")
    source = call_llm(full_prompt)

    if source.startswith("Error:"):
        error_resp = RESP_START + source.encode() + RESP_END + b"\n"
        sock.sendall(error_resp)
        print(f"[SERIAL-PROXY] Gemini error: {source}")
        return

    # Step 2: Strip markdown fences if present
    if "```rust" in source:
        source = source.split("```rust")[1].split("```")[0]
    elif "```" in source:
        source = source.split("```")[1].split("```")[0]
    source = source.strip()

    print(f"[SERIAL-PROXY] Generated {len(source)} chars of Rust source")

    # Step 3: Create temp Cargo project
    tmp_dir = tempfile.mkdtemp(prefix="folkwasm_")
    try:
        # cargo new --lib
        proj_dir = os.path.join(tmp_dir, "wasm_tool")
        subprocess.run(["cargo", "new", "--lib", proj_dir], capture_output=True, timeout=10)

        # Write Cargo.toml with aggressive optimization
        cargo_toml = f"""[package]
name = "wasm_tool"
version = "0.1.0"
edition = "2021"

[lib]
crate-type = ["cdylib"]

[profile.release]
opt-level = "z"
lto = true
strip = true
codegen-units = 1
panic = "abort"
"""
        with open(os.path.join(proj_dir, "Cargo.toml"), "w") as f:
            f.write(cargo_toml)

        # Write source
        src_path = os.path.join(proj_dir, "src", "lib.rs")
        with open(src_path, "w") as f:
            f.write(source)

        # Step 4: Compile
        print("[SERIAL-PROXY] Compiling WASM...")
        result = subprocess.run(
            ["cargo", "build", "--target", "wasm32-unknown-unknown", "--release"],
            capture_output=True, text=True, timeout=60,
            cwd=proj_dir,
        )

        if result.returncode != 0:
            error = f"Compile error: {result.stderr[:400]}"
            print(f"[SERIAL-PROXY] {error}")
            error_resp = RESP_START + error.encode() + RESP_END + b"\n"
            sock.sendall(error_resp)
            return

        # Find compiled WASM
        wasm_path = os.path.join(
            proj_dir, "target", "wasm32-unknown-unknown", "release", "wasm_tool.wasm"
        )
        if not os.path.exists(wasm_path):
            error_resp = RESP_START + b"Compile error: WASM output not found" + RESP_END + b"\n"
            sock.sendall(error_resp)
            return

        with open(wasm_path, "rb") as f:
            wasm_binary = f.read()

        print(f"[SERIAL-PROXY] WASM compiled: {len(wasm_binary)} bytes")

        # Step 5: Size check
        if len(wasm_binary) > MAX_WASM_SIZE:
            error = f"WASM too large: {len(wasm_binary)} bytes (max {MAX_WASM_SIZE})"
            error_resp = RESP_START + error.encode() + RESP_END + b"\n"
            sock.sendall(error_resp)
            return

        # Step 6: Base64 encode
        b64_data = base64.b64encode(wasm_binary).decode("ascii")

        # Step 7: Send response
        resp_json = json.dumps({"action": "tool_ready", "binary": b64_data})
        response = RESP_START + resp_json.encode() + RESP_END + b"\n"
        sock.sendall(response)
        print(f"[SERIAL-PROXY] Sent tool_ready: {len(wasm_binary)} bytes WASM, {len(b64_data)} chars base64")

    except subprocess.TimeoutExpired:
        error_resp = RESP_START + b"Compile timeout (60s)" + RESP_END + b"\n"
        sock.sendall(error_resp)
        print("[SERIAL-PROXY] Compile timeout")
    except Exception as e:
        error = f"Tool generation error: {e}"
        print(f"[SERIAL-PROXY] {error}")
        error_resp = RESP_START + error.encode() + RESP_END + b"\n"
        sock.sendall(error_resp)
    finally:
        # Cleanup temp directory
        try:
            shutil.rmtree(tmp_dir)
        except Exception:
            pass


def call_llm(prompt: str) -> str:
    """Call the configured LLM provider and return response text."""
    try:
        if LLM_PROVIDER == "gemini":
            return _call_gemini(prompt)
        elif LLM_PROVIDER == "openai":
            return _call_openai(prompt)
        elif LLM_PROVIDER == "claude":
            return _call_claude(prompt)
        else:
            return f"Error: Unknown provider '{LLM_PROVIDER}'"
    except Exception as e:
        return f"Error: {e}"


def _call_gemini(prompt: str) -> str:
    """Google Gemini API."""
    url = f"{LLM_BASE_URL}/models/{LLM_MODEL}:generateContent?key={LLM_API_KEY}"
    body = json.dumps({
        "contents": [{"parts": [{"text": prompt}]}]
    }).encode()
    req = urllib.request.Request(url, data=body,
        headers={"Content-Type": "application/json"}, method="POST")
    ctx = ssl.create_default_context()
    with urllib.request.urlopen(req, context=ctx, timeout=60) as resp:
        result = json.loads(resp.read())
        return result["candidates"][0]["content"]["parts"][0]["text"]


def _call_openai(prompt: str) -> str:
    """OpenAI / ChatGPT API (also works with compatible APIs like Ollama, LM Studio)."""
    url = f"{LLM_BASE_URL}/chat/completions"
    body = json.dumps({
        "model": LLM_MODEL,
        "messages": [{"role": "user", "content": prompt}],
        "max_tokens": 4096,
    }).encode()
    req = urllib.request.Request(url, data=body, headers={
        "Content-Type": "application/json",
        "Authorization": f"Bearer {LLM_API_KEY}",
    }, method="POST")
    ctx = ssl.create_default_context()
    with urllib.request.urlopen(req, context=ctx, timeout=60) as resp:
        result = json.loads(resp.read())
        return result["choices"][0]["message"]["content"]


def _call_claude(prompt: str) -> str:
    """Anthropic Claude API."""
    url = f"{LLM_BASE_URL}/messages"
    body = json.dumps({
        "model": LLM_MODEL,
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


def handle_serial(sock: socket.socket):
    """Read from COM2, process requests, write responses."""
    buf = b""
    print("[SERIAL-PROXY] Connected to COM2, listening for requests...")

    while True:
        try:
            data = sock.recv(4096)
            if not data:
                print("[SERIAL-PROXY] COM2 disconnected")
                break

            buf += data

            # Look for complete request: @@GEMINI_REQ@@{...}@@END@@
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
